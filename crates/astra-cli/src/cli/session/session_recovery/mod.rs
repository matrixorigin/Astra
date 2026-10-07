//! Session recovery: checkpoint, workspace, CSL, and I/O primitives.
//! Sub-modules split by concern to keep files manageable.

pub(crate) mod csl;
pub(crate) mod io;
pub(crate) mod workspace;

// Re-export public items from sub-modules

pub(crate) use workspace::{
    context_trace_signal_from_trace, sync_context_trace_to_workspace,
    sync_session_state_to_workspace, workspace_metadata_from_live_state_after_read_failure,
};

#[cfg(test)]
pub(crate) use workspace::session_workspace_git_root;

#[cfg(test)]
mod tests {
    use super::csl::write_full_csl_snapshot_atomic;
    use super::io::{csl_log_path_for, write_bytes_atomic};
    use super::{
        session_workspace_git_root, sync_context_trace_to_workspace,
        sync_session_state_to_workspace, workspace::workspace_metadata_from_live_state,
    };
    use crate::cli::session::session_state::SessionState;
    use astra_services::session_journal;

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
    #[serial_test::serial]
    fn workspace_metadata_from_live_state_rebuilds_missing_workspace() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let sid = format!("workspace-live-missing-{}", uuid::Uuid::new_v4());
        let state = SessionState {
            session_id: Some(sid.clone()),
            model: Some(("gpt-5".to_string()).into()),
            turn: 3,
            total_prompt_tokens: 111,
            total_completion_tokens: 222,
            total_cache_read_tokens: 33,
            total_cache_creation_tokens: 44,
            ..Default::default()
        };

        let ws = workspace_metadata_from_live_state(&state, &sid);
        assert_eq!(ws.session_id, sid);
        assert_eq!(ws.turn_count, 3);
        assert_eq!(ws.total_tokens_in, 111);
        assert_eq!(ws.total_tokens_out, 222);
        assert_eq!(ws.total_cache_read_tokens, 33);
        assert_eq!(ws.total_cache_creation_tokens, 44);
        assert_eq!(ws.status, "active");
        assert_eq!(ws.model.as_deref(), Some("gpt-5"));
    }

    #[test]
    #[serial_test::serial]
    fn workspace_metadata_from_live_state_recovers_from_corrupt_workspace() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let sid = format!("workspace-live-corrupt-{}", uuid::Uuid::new_v4());
        let mut persisted =
            astra_services::session_workspace::WorkspaceMetadata::new(&sid, "gpt-4");
        persisted.git_root = Some("/repo".to_string());
        astra_services::session_workspace::write_workspace(&persisted).unwrap();
        let workspace_path = astra_services::session_workspace::workspace_file_path(&sid).unwrap();
        let corrupt_bytes = b":\nnot-valid-yaml".to_vec();
        std::fs::write(&workspace_path, &corrupt_bytes).unwrap();

        let state = SessionState {
            session_id: Some(sid.clone()),
            model: Some(("gpt-5".to_string()).into()),
            turn: 4,
            total_prompt_tokens: 500,
            total_completion_tokens: 250,
            total_cache_read_tokens: 80,
            total_cache_creation_tokens: 20,
            ..Default::default()
        };

        let ws = workspace_metadata_from_live_state(&state, &sid);
        assert_eq!(ws.session_id, sid);
        assert_eq!(ws.turn_count, 4);
        assert_eq!(ws.total_tokens_in, 500);
        assert_eq!(ws.total_tokens_out, 250);
        assert_eq!(ws.total_cache_read_tokens, 80);
        assert_eq!(ws.total_cache_creation_tokens, 20);
        assert_eq!(ws.status, "active");
        assert_eq!(ws.model.as_deref(), Some("gpt-5"));
        assert!(!ws.cwd.is_empty());
        assert!(!ws.created_at.is_empty());
        let backup =
            workspace_backup_path_for(&sid).expect("corrupt workspace should be backed up");
        assert_eq!(std::fs::read(backup).unwrap(), corrupt_bytes);
    }

    #[test]
    #[serial_test::serial]
    fn workspace_metadata_from_live_state_recovers_checkpoint_turns_from_index() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let sid = format!("workspace-live-checkpoints-{}", uuid::Uuid::new_v4());
        let checkpoint_dir =
            astra_services::session_workspace::workspace_dir_for(&sid).join("checkpoints");
        std::fs::create_dir_all(&checkpoint_dir).unwrap();
        std::fs::write(
            checkpoint_dir.join("index.md"),
            "# Checkpoint Index\n\n  001 - Turn  3 - First\n  002 - Turn  6 - Second\n",
        )
        .unwrap();

        let state = SessionState {
            session_id: Some(sid.clone()),
            model: Some(("gpt-5".to_string()).into()),
            turn: 7,
            total_prompt_tokens: 700,
            total_completion_tokens: 300,
            ..Default::default()
        };

        let ws = workspace_metadata_from_live_state(&state, &sid);
        assert_eq!(ws.checkpoints, vec![3, 6]);
    }

    #[test]
    #[serial_test::serial]
    fn workspace_metadata_from_live_state_preserves_monotonic_counters() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let sid = format!("workspace-live-monotonic-{}", uuid::Uuid::new_v4());
        let mut persisted =
            astra_services::session_workspace::WorkspaceMetadata::new(&sid, "gpt-4");
        persisted.turn_count = 5;
        persisted.total_tokens_in = 500;
        persisted.total_tokens_out = 250;
        persisted.total_cache_read_tokens = 80;
        persisted.total_cache_creation_tokens = 20;
        astra_services::session_workspace::write_workspace(&persisted).unwrap();

        let state = SessionState {
            session_id: Some(sid.clone()),
            model: Some(("gpt-5".to_string()).into()),
            turn: 3,
            total_prompt_tokens: 100,
            total_completion_tokens: 50,
            total_cache_read_tokens: 10,
            total_cache_creation_tokens: 5,
            ..Default::default()
        };

        let ws = workspace_metadata_from_live_state(&state, &sid);
        assert_eq!(ws.turn_count, 5);
        assert_eq!(ws.total_tokens_in, 500);
        assert_eq!(ws.total_tokens_out, 250);
        assert_eq!(ws.total_cache_read_tokens, 80);
        assert_eq!(ws.total_cache_creation_tokens, 20);
    }

    #[test]
    #[serial_test::serial]
    fn workspace_metadata_from_live_state_recovers_counters_from_journal() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let sid = format!("workspace-live-journal-{}", uuid::Uuid::new_v4());
        let writer = session_journal::JournalWriter::new(&sid).unwrap();
        writer
            .append(&session_journal::JournalEvent::session_start(
                Some(&sid),
                Some("gpt-5"),
            ))
            .unwrap();
        writer
            .append(&session_journal::JournalEvent::turn(
                Some(&sid),
                2,
                Some("gpt-5"),
                "continue",
                "done",
                0,
                120,
                45,
                30,
            ))
            .unwrap();

        let state = SessionState {
            session_id: Some(sid.clone()),
            model: Some(("gpt-5".to_string()).into()),
            ..Default::default()
        };

        let ws = workspace_metadata_from_live_state(&state, &sid);
        assert_eq!(ws.turn_count, 2);
        assert_eq!(ws.total_tokens_in, 120);
        assert_eq!(ws.total_tokens_out, 45);
    }

    #[test]
    fn sync_context_trace_copies_latest_trace_into_workspace() {
        let mut state = SessionState::default();
        let mut obs = astra_runtime::observability::ObservabilitySession::new_simple("sid-trace");
        obs.context_traces
            .push(astra_turn_core::context_assembly_trace::ContextAssemblyTrace {
                turn_id: "turn-3".into(),
                tools: astra_turn_core::context_assembly_trace::ToolSurfaceTrace {
                    visible_tools: vec![astra_turn_core::context_assembly_trace::VisibleTool {
                        tool_name: "lsp".into(),
                        tokens: 0,
                    }],
                    ..Default::default()
                },
                memory: astra_turn_core::context_assembly_trace::MemoryRetrievalTrace {
                    outcome: astra_turn_types::MemoryRetrievalOutcome::Complete,
                    query: "resume trace persistence".into(),
                    memories_selected: vec![astra_turn_core::context_assembly_trace::MemorySelection {
                        memory_id: "m1".into(),
                        memory_type: "semantic".into(),
                        content_preview: "trace".into(),
                        relevance_score: 0.8,
                        tokens: 10,
                        source: astra_turn_core::context_assembly_trace::MemorySource::Memoria,
                    }],
                    ..Default::default()
                },
                history: astra_turn_core::context_assembly_trace::HistorySelectionTrace {
                    turns_compressed: vec![astra_turn_core::context_assembly_trace::TurnCompression {
                        turn_index: 1,
                        role: "assistant".into(),
                        original_tokens: 100,
                        compressed_tokens: 50,
                        compression_method:
                            astra_turn_core::context_assembly_trace::CompressionMethod::ReactiveCompact,
                        information_lost: Vec::new(),
                    }],
                    compression_ratio: 0.5,
                    tokens_before: 100,
                    tokens_after: 50,
                    ..Default::default()
                },
                token_budget: astra_turn_core::context_assembly_trace::TokenBudgetTrace {
                    max_tokens: 16_000,
                    total_used: 8_200,
                    budget_pressure: 0.76,
                    ..Default::default()
                },
                explanations: vec![astra_turn_core::context_assembly_trace::DecisionExplanation {
                    decision_type:
                        astra_turn_core::context_assembly_trace::DecisionType::StrategyChoice {
                            strategy: "symbol_context".into(),
                        },
                    reasoning: "Need symbol-aware context.".into(),
                    alternatives_considered: Vec::new(),
                    confidence: 0.8,
                }],
                ..Default::default()
            });
        state.observability_session = Some(std::sync::Arc::new(std::sync::RwLock::new(obs)));

        let mut ws = astra_services::session_workspace::WorkspaceMetadata::new("sid-trace", "m");
        sync_context_trace_to_workspace(&state, &mut ws);

        let trace = ws.last_context_trace.expect("missing trace summary");
        assert_eq!(trace.turn_id, "turn-3");
        assert_eq!(
            trace
                .tool_surface
                .as_ref()
                .map(|selection| selection.visible_tools.clone()),
            Some(vec!["lsp".to_string()])
        );
        assert_eq!(
            trace
                .tool_surface
                .as_ref()
                .map(|selection| selection.surface_scope.as_str()),
            Some("latest_round")
        );
        assert_eq!(
            trace
                .memory
                .as_ref()
                .map(|memory| memory.selected_memory_ids.len()),
            Some(1)
        );
        assert_eq!(
            trace.budget.as_ref().map(|budget| budget.total_used),
            Some(8_200)
        );
    }

    #[test]
    fn write_bytes_atomic_replaces_existing_contents() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("atom.txt");
        std::fs::write(&path, b"old").unwrap();

        write_bytes_atomic(&path, b"new", "test atomic write").unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        assert!(
            !tmp.path().join(".tmp-atom.txt").exists(),
            "temporary file should not survive atomic replace"
        );
    }

    #[test]
    fn write_bytes_atomic_surfaces_temporary_path_conflict() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("atom.txt");
        std::fs::create_dir(tmp.path().join(".tmp-atom.txt")).unwrap();

        let error = write_bytes_atomic(&path, b"new", "test atomic write")
            .expect_err("directory conflict should fail atomic write");

        assert!(error.contains("create temporary file"), "{error}");
        assert!(!path.exists(), "failed atomic write must not create target");
    }

    #[test]
    fn write_full_csl_snapshot_atomic_persists_snapshot_without_tmp_file() {
        let (_tmp, _g) = crate::tests::isolated_sessions_dir();
        let sid = format!("write-csl-{}", uuid::Uuid::new_v4());
        let messages = vec![
            serde_json::json!({"role": "user", "content": "hello"}),
            serde_json::json!({"role": "assistant", "content": "world"}),
        ];
        let session_state = astra_turn_core::conversation_log::SessionStateCompact {
            recent_tools: vec!["bash".into()],
            ..Default::default()
        };

        write_full_csl_snapshot_atomic(&sid, 2, &messages, &session_state).unwrap();

        let csl_path = csl_log_path_for(&sid);
        assert!(csl_path.exists());
        assert!(
            !csl_path
                .parent()
                .unwrap()
                .join(".tmp-conversation_log.jsonl")
                .exists(),
            "temporary file should not survive atomic write"
        );

        let line = std::fs::read_to_string(&csl_path)
            .unwrap()
            .lines()
            .next()
            .unwrap()
            .to_string();
        let entry: astra_turn_core::conversation_log::CslEntry =
            serde_json::from_str(&line).unwrap();
        match entry {
            astra_turn_core::conversation_log::CslEntry::Snapshot {
                turn,
                messages: restored,
                session_state: restored_state,
                ..
            } => {
                assert_eq!(turn, 2);
                assert_eq!(restored, messages);
                assert_eq!(restored_state.recent_tools, vec!["bash".to_string()]);
            }
            other => panic!("expected snapshot entry, got {other:?}"),
        }
    }

    #[test]
    fn write_full_csl_snapshot_atomic_increments_seq_past_existing_log() {
        // Regression: the recovery snapshot used to hardcode `seq: 1`. If a
        // session already had snapshots/deltas at seq>=1, writing a new
        // recovery snapshot at seq=1 broke the strictly-increasing seq
        // invariant required by `materialize_session_state` and rendered any
        // out-of-band consumer relying on seq monotonicity inconsistent.
        // The snapshot sequence MUST exceed the highest seq present in the
        // existing log.
        let (_tmp, _g) = crate::tests::isolated_sessions_dir();
        let sid = format!("csl-seq-{}", uuid::Uuid::new_v4());
        let messages_pre = vec![serde_json::json!({"role":"user","content":"prior"})];
        let state = astra_turn_core::conversation_log::SessionStateCompact::default();

        // Simulate an established log: snapshot(seq=1) + a couple of deltas.
        let csl_path = csl_log_path_for(&sid);
        std::fs::create_dir_all(csl_path.parent().unwrap()).unwrap();
        let mut log_text = String::new();
        let snap1 = astra_turn_core::conversation_log::CslEntry::Snapshot {
            seq: 1,
            turn: 0,
            messages: messages_pre.clone(),
            session_state: state.clone(),
        };
        log_text.push_str(&serde_json::to_string(&snap1).unwrap());
        log_text.push('\n');
        let delta_a = astra_turn_core::conversation_log::CslEntry::TurnDelta {
            seq: 2,
            turn: 1,
            appended: vec![serde_json::json!({"role":"assistant","content":"a"})],
            state_patch: None,
        };
        let delta_b = astra_turn_core::conversation_log::CslEntry::TurnDelta {
            seq: 3,
            turn: 2,
            appended: vec![serde_json::json!({"role":"user","content":"b"})],
            state_patch: None,
        };
        log_text.push_str(&serde_json::to_string(&delta_a).unwrap());
        log_text.push('\n');
        log_text.push_str(&serde_json::to_string(&delta_b).unwrap());
        log_text.push('\n');
        std::fs::write(&csl_path, log_text).unwrap();

        let messages_now = vec![serde_json::json!({"role":"user","content":"after-recovery"})];
        write_full_csl_snapshot_atomic(&sid, 3, &messages_now, &state).unwrap();

        let read = std::fs::read_to_string(&csl_path).unwrap();
        let line = read.lines().next().expect("at least one line");
        let entry: astra_turn_core::conversation_log::CslEntry =
            serde_json::from_str(line).unwrap();
        match entry {
            astra_turn_core::conversation_log::CslEntry::Snapshot { seq, turn, .. } => {
                assert!(
                    seq > 3,
                    "snapshot seq must exceed prior log's max seq (3), got {seq}"
                );
                assert_eq!(turn, 3);
            }
            other => panic!("expected Snapshot, got {other:?}"),
        }
    }

    #[test]
    #[serial_test::serial]
    fn session_workspace_git_root_returns_root_when_workspace_exists() {
        let (_tmp, _g) = crate::tests::isolated_sessions_dir();
        let sid = format!("git-root-ok-{}", uuid::Uuid::new_v4());
        let mut workspace =
            astra_services::session_workspace::WorkspaceMetadata::new(&sid, "gpt-5");
        workspace.git_root = Some("/repo".to_string());
        astra_services::session_workspace::write_workspace(&workspace).unwrap();

        assert_eq!(
            session_workspace_git_root(Some(&sid)).as_deref(),
            Some("/repo")
        );
    }

    #[test]
    #[serial_test::serial]
    fn session_workspace_git_root_returns_none_for_invalid_workspace() {
        let (_tmp, _g) = crate::tests::isolated_sessions_dir();
        let sid = format!("git-root-bad-{}", uuid::Uuid::new_v4());
        let mut workspace =
            astra_services::session_workspace::WorkspaceMetadata::new(&sid, "gpt-5");
        workspace.git_root = Some("/repo".to_string());
        astra_services::session_workspace::write_workspace(&workspace).unwrap();
        let workspace_path = astra_services::session_workspace::workspace_file_path(&sid).unwrap();
        std::fs::write(&workspace_path, ":\nnot-valid-yaml").unwrap();

        assert!(session_workspace_git_root(Some(&sid)).is_none());
    }

    #[test]
    fn sync_session_state_to_workspace_preserves_workspace_history_and_config() {
        let mut state = SessionState::default();
        state.session_persistence_error = Some("journal append failed".to_string());

        let mut ws = astra_services::session_workspace::WorkspaceMetadata::new("sid-adaptive", "m");
        ws.discovered_skills = vec!["skill-b".to_string()];
        sync_session_state_to_workspace(&state, &mut ws);

        assert_eq!(
            ws.last_persistence_error.as_deref(),
            Some("journal append failed")
        );
        assert_eq!(ws.discovered_skills, vec!["skill-b".to_string()]);
        assert!(
            ws.tuned_config_json.is_none(),
            "workspace config remains authoritative during recovery projection"
        );
    }
}
