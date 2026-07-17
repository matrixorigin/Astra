use crate::cli::project_instructions::format_project_instructions;
use crate::cli::session::session_state::SessionState;
use astra_runtime::prompts;
use astra_tools::task_mgmt::{SessionTask, unresolved_task_blocker_ids};
use astra_turn_core::input_classifier;

/// Detect if a user message appears to be a correction/redirection.
pub(crate) fn detect_correction_signal(message: &str) -> bool {
    input_classifier::is_reanchor_signal(message)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FinalizedInput {
    pub(crate) user_message: String,
    pub(crate) user_intent: String,
    /// External/session-recovery context required for the next turn.
    pub(crate) runtime_required_texts: Vec<String>,
    /// Dynamic text from external session sources such as task-board
    /// snapshots. Internal runtime state uses required/typed lanes.
    pub(crate) runtime_volatile_texts: Vec<String>,
}

pub(crate) fn clear_pending_recovery_for_ordinary_chat_input(state: &mut SessionState) {
    state.pending_recovery = None;
    state.resume_restricted_tools.clear();
}

pub(crate) async fn finalize_effective_line(
    effective_line: String,
    user_intent: String,
    resume_guidance: Option<String>,
    state: &mut SessionState,
) -> FinalizedInput {
    state.diagnostics_context = None;
    let mut runtime_required_texts = Vec::new();
    let mut runtime_volatile_texts = Vec::new();

    if !state.pending_bg_notifications.is_empty() {
        let notifications = state
            .pending_bg_notifications
            .drain(..)
            .collect::<Vec<_>>()
            .join("\n");
        runtime_required_texts.push(format!(
            "Background task updates since your last turn:\n{notifications}"
        ));
    }

    const TURNS_SINCE_TASK_USE_THRESHOLD: u32 = 10;
    const TURNS_BETWEEN_REMINDERS: u32 = 10;
    if state.recent_tools.iter().any(|tool| tool == "task_board") {
        state.turns_since_task_use = 0;
    } else {
        state.turns_since_task_use += 1;
    }
    state.turns_since_task_reminder += 1;

    if state.turns_since_task_use >= TURNS_SINCE_TASK_USE_THRESHOLD
        && state.turns_since_task_reminder >= TURNS_BETWEEN_REMINDERS
    {
        let snapshot = match state.task_manager.load_tasks().await {
            Ok(tasks) => format_open_task_snapshot(&tasks)
                .map(|task_list| format!("External task board snapshot:\n{task_list}")),
            Err(error) => Some(format!("External task board snapshot unavailable: {error}")),
        };
        if let Some(snapshot) = snapshot {
            runtime_volatile_texts.push(snapshot);
        }
        state.turns_since_task_reminder = 0;
    }

    if let Some(guidance) = resume_guidance
        && !guidance.trim().is_empty()
    {
        runtime_required_texts.push(guidance);
    }

    FinalizedInput {
        user_message: effective_line,
        user_intent,
        runtime_required_texts,
        runtime_volatile_texts,
    }
}

fn compact_blocker_ids(blockers: &[String]) -> String {
    const MAX_IDS: usize = 3;
    let mut ids = blockers.iter().take(MAX_IDS).cloned().collect::<Vec<_>>();
    if blockers.len() > MAX_IDS {
        ids.push(format!("+{} more", blockers.len() - MAX_IDS));
    }
    ids.join(", ")
}

fn format_open_task_snapshot(tasks: &[SessionTask]) -> Option<String> {
    let mut open = tasks
        .iter()
        .filter(|task| task.status.is_open_work())
        .map(|task| (task, unresolved_task_blocker_ids(tasks, task)))
        .collect::<Vec<_>>();
    open.sort_by_key(|(task, blockers)| (task.status.active_priority(), !blockers.is_empty()));
    let mut lines: Vec<String> = open
        .iter()
        .take(10)
        .map(|(task, blockers)| {
            let title = task.title.chars().take(120).collect::<String>();
            let blocked = if blockers.is_empty() {
                String::new()
            } else {
                format!(" [blocked by: {}]", compact_blocker_ids(blockers))
            };
            format!("- [{}] {}: {}{}", task.status, task.id, title, blocked)
        })
        .collect();
    let open_total = open.len();
    if open_total > lines.len() {
        lines.push(format!(
            "- ... {} more open task(s)",
            open_total - lines.len()
        ));
    }
    if lines.is_empty() {
        None
    } else {
        Some(lines.join("\n"))
    }
}

pub(crate) fn build_effective_line(
    line: &str,
    state: &SessionState,
    ui: &mut dyn crate::cli::ui_adapter::ReplUiAdapter,
) -> String {
    let mut effective_line = if let Some(skill_dev) = state.skill_dev.as_ref() {
        let skill_md = skill_dev.dir.join("SKILL.md");
        match std::fs::read_to_string(&skill_md) {
            Ok(source) if !source.trim().is_empty() => format!(
                "{}{line}",
                prompts::build_skill_dev_prefix(
                    &skill_dev.name,
                    &skill_md.display().to_string(),
                    &source,
                )
            ),
            Ok(_) => {
                ui.show_warning(&format!(
                    "  ⚠ SKILL.md is empty at {}, dev context skipped",
                    skill_md.display()
                ));
                line.to_string()
            }
            Err(_) => {
                ui.show_warning(&format!(
                    "  ⚠ SKILL.md not found at {}, dev context skipped",
                    skill_md.display()
                ));
                line.to_string()
            }
        }
    } else {
        line.to_string()
    };

    if !state.active_system_skills.is_empty() {
        let skill_block = prompts::build_skill_instructions(&state.active_system_skills);
        effective_line = format!("{skill_block}\n\n{effective_line}");
    }

    if let Some(diagnostics_context) = state.diagnostics_context.as_ref() {
        effective_line = format!("{diagnostics_context}\n\n{effective_line}");
    }

    if let Some(project_instructions) = state.project_instructions.as_ref() {
        let block = format_project_instructions(project_instructions);
        effective_line = format!("{block}\n\n{effective_line}");
    }

    effective_line
}

#[cfg(test)]
mod tests {
    use super::{
        build_effective_line, clear_pending_recovery_for_ordinary_chat_input,
        detect_correction_signal, finalize_effective_line,
    };
    use crate::cli::session::session_state::SessionState;
    use crate::cli::session::session_state::SkillDevState;
    use astra_runtime::prompts;
    use astra_tools::task_mgmt::{SessionTask, TaskManager, TaskMutation, TaskStore};

    struct FailingTaskLoadStore;

    #[async_trait::async_trait]
    impl TaskStore for FailingTaskLoadStore {
        async fn load(&self, session_id: &str) -> Result<Vec<SessionTask>, String> {
            Err(format!("forced task load failure for {session_id}"))
        }

        async fn save(&self, _session_id: &str, _tasks: Vec<SessionTask>) -> Result<(), String> {
            Ok(())
        }

        async fn mutate(
            &self,
            session_id: &str,
            _mutation: TaskMutation,
        ) -> Result<String, String> {
            Err(format!("forced task mutate failure for {session_id}"))
        }

        async fn next_task_id(&self, session_id: &str) -> Result<u32, String> {
            Err(format!("forced next task id failure for {session_id}"))
        }

        async fn peek_next_task_id(&self, _session_id: &str) -> Result<u32, String> {
            Ok(1)
        }
    }

    #[test]
    fn detect_correction_signal_handles_english_and_chinese_redirects() {
        assert!(detect_correction_signal("No, that's wrong."));
        assert!(detect_correction_signal("不对，我的意思是改这里"));
        assert!(!detect_correction_signal("please continue with the fix"));
    }

    #[test]
    fn build_effective_line_does_not_phrase_match_short_continue() {
        let state = SessionState {
            continuation_anchor: Some(
                "Latest user task: debug Chinese input drops\nLatest assistant direction: inspect prompt redraw path"
                    .to_string()
                    .into(),
            ),
            ..SessionState::default()
        };

        let effective =
            build_effective_line("继续", &state, &mut crate::cli::ui_adapter::LineUiAdapter);
        assert!(!effective.contains("[Active task attachment]"));
        assert!(!effective.contains("debug Chinese input drops"));
        assert_eq!(effective, "继续");
    }

    #[test]
    fn build_effective_line_does_not_reanchor_repair_followup_by_phrase() {
        let state = SessionState {
            continuation_anchor: Some(
                "Latest user task: review commit aa1f419b\nLatest assistant summary:\n## Review\nP5 still blocks large merges"
                    .into(),
            ),
            ..SessionState::default()
        };

        let effective =
            build_effective_line("修复?", &state, &mut crate::cli::ui_adapter::LineUiAdapter);
        assert_eq!(effective, "修复?");
    }

    #[test]
    fn build_effective_line_does_not_reanchor_generic_followup_to_task_board() {
        let state = SessionState {
            continuation_anchor: Some(
                "Latest user task: improve session memory flow\nActive task board:\n- [in_progress] task-1: Phase 1: /memory show — TDD".into(),
            ),
            ..SessionState::default()
        };

        let effective = build_effective_line(
            "还有什么？",
            &state,
            &mut crate::cli::ui_adapter::LineUiAdapter,
        );
        assert_eq!(effective, "还有什么？");
    }

    #[test]
    fn build_effective_line_leaves_normal_prompt_untouched() {
        let state = SessionState {
            continuation_anchor: Some("Latest user task: debug Chinese input drops".into()),
            ..SessionState::default()
        };

        let effective = build_effective_line(
            "修一下输入法问题",
            &state,
            &mut crate::cli::ui_adapter::LineUiAdapter,
        );
        assert!(!effective.contains("[Active task attachment]"));
        assert_eq!(effective, "修一下输入法问题");
    }

    #[tokio::test]
    async fn finalize_effective_line_routes_resume_guidance_to_required_lane() {
        let mut state = SessionState::default();

        let finalized = finalize_effective_line(
            "continue".into(),
            "raw continue".into(),
            Some("Resume the interrupted turn before answering.".into()),
            &mut state,
        )
        .await;

        assert_eq!(finalized.user_message, "continue");
        assert_eq!(finalized.user_intent, "raw continue");
        assert_eq!(
            finalized.runtime_required_texts,
            vec!["Resume the interrupted turn before answering.".to_string()]
        );
        assert!(finalized.runtime_volatile_texts.is_empty());
        assert!(!finalized.user_message.contains("<system-reminder>"));
        assert!(!finalized.user_message.contains("[session-resume:v1]"));
    }

    #[test]
    fn build_effective_line_skill_dev_reads_from_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let skill_dir = tmp.path().join("test-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: test-skill\n---\n# Test\nDo stuff.",
        )
        .unwrap();

        let state = SessionState {
            skill_dev: Some(SkillDevState {
                name: "test-skill".to_string(),
                dir: skill_dir,
            }),
            ..SessionState::default()
        };

        let effective = build_effective_line(
            "improve this skill",
            &state,
            &mut crate::cli::ui_adapter::LineUiAdapter,
        );
        assert!(effective.contains("[SKILL DEV: test-skill]"));
        assert!(effective.contains("Do stuff."));
        assert!(effective.contains("improve this skill"));
    }

    #[test]
    fn build_effective_line_skill_dev_picks_up_external_edits() {
        const OLD_BODY: &str = "skill body version one";
        const NEW_BODY: &str = "skill body version two rewritten";
        let tmp = tempfile::tempdir().unwrap();
        let skill_dir = tmp.path().join("evolving");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            format!("---\nname: evolving\n---\n{OLD_BODY}"),
        )
        .unwrap();

        let state = SessionState {
            skill_dev: Some(SkillDevState {
                name: "evolving".to_string(),
                dir: skill_dir.clone(),
            }),
            ..SessionState::default()
        };

        let turn1 =
            build_effective_line("check", &state, &mut crate::cli::ui_adapter::LineUiAdapter);
        assert!(turn1.contains(OLD_BODY));

        std::fs::write(
            skill_dir.join("SKILL.md"),
            format!("---\nname: evolving\n---\n{NEW_BODY}"),
        )
        .unwrap();

        let turn2 = build_effective_line(
            "check again",
            &state,
            &mut crate::cli::ui_adapter::LineUiAdapter,
        );
        assert!(
            !turn2.contains(OLD_BODY),
            "should not contain old skill body"
        );
        assert!(turn2.contains(NEW_BODY), "should contain new content");
    }

    #[test]
    fn build_effective_line_skill_dev_missing_file_falls_through() {
        let state = SessionState {
            skill_dev: Some(SkillDevState {
                name: "ghost".to_string(),
                dir: std::path::PathBuf::from("/nonexistent/path/ghost"),
            }),
            ..SessionState::default()
        };

        let effective =
            build_effective_line("hello", &state, &mut crate::cli::ui_adapter::LineUiAdapter);
        assert_eq!(effective, "hello");
    }

    #[test]
    fn build_effective_line_skill_dev_empty_file_falls_through() {
        let tmp = tempfile::tempdir().unwrap();
        let skill_dir = tmp.path().join("empty-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(skill_dir.join("SKILL.md"), "").unwrap();

        let state = SessionState {
            skill_dev: Some(SkillDevState {
                name: "empty-skill".to_string(),
                dir: skill_dir,
            }),
            ..SessionState::default()
        };

        let effective =
            build_effective_line("hello", &state, &mut crate::cli::ui_adapter::LineUiAdapter);
        assert_eq!(effective, "hello");
    }

    #[test]
    fn build_effective_line_skill_dev_shows_actual_path() {
        let tmp = tempfile::tempdir().unwrap();
        let skill_dir = tmp.path().join("custom-loc");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: custom-loc\n---\nBody",
        )
        .unwrap();

        let state = SessionState {
            skill_dev: Some(SkillDevState {
                name: "custom-loc".to_string(),
                dir: skill_dir.clone(),
            }),
            ..SessionState::default()
        };

        let effective =
            build_effective_line("x", &state, &mut crate::cli::ui_adapter::LineUiAdapter);
        let expected_path = skill_dir.join("SKILL.md").display().to_string();
        assert!(
            effective.contains(&expected_path),
            "should contain actual path: {expected_path}"
        );
    }

    #[test]
    fn build_effective_line_skill_dev_combines_with_system_skills_and_anchor() {
        let tmp = tempfile::tempdir().unwrap();
        let skill_dir = tmp.path().join("combo");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: combo\n---\nCombo skill",
        )
        .unwrap();

        let state = SessionState {
            skill_dev: Some(SkillDevState {
                name: "combo".to_string(),
                dir: skill_dir,
            }),
            active_system_skills: vec![prompts::builtin_concise_skill()],
            continuation_anchor: Some("Previous task: fix auth".into()),
            ..SessionState::default()
        };

        let effective = build_effective_line(
            "continue",
            &state,
            &mut crate::cli::ui_adapter::LineUiAdapter,
        );
        assert!(effective.contains("[SKILL DEV: combo]"), "skill dev prefix");
        assert!(effective.contains("Concise"), "system skill");
        assert!(!effective.contains("[Active task attachment]"), "anchor");
        assert!(!effective.contains("fix auth"), "anchor content");
    }

    #[test]
    fn clear_pending_recovery_for_ordinary_chat_input_drops_resume_state() {
        let mut state = SessionState {
            pending_recovery: Some("sess-stale".into()),
            resume_restricted_tools: vec!["read_file".into(), "bash".into()],
            ..SessionState::default()
        };

        clear_pending_recovery_for_ordinary_chat_input(&mut state);

        assert!(state.pending_recovery.is_none());
        assert!(state.resume_restricted_tools.is_empty());
    }

    #[tokio::test]
    async fn finalize_effective_line_drains_notifications_without_mutating_user_message() {
        let mut state = SessionState {
            diagnostics_context: Some("<diag/>".into()),
            pending_bg_notifications: vec![
                "bg-shell-1 completed".into(),
                "bg-shell-2 failed".into(),
            ],
            ..SessionState::default()
        };

        let finalized = finalize_effective_line(
            "continue".into(),
            "continue".into(),
            Some("Resume the interrupted task.".into()),
            &mut state,
        )
        .await;

        assert_eq!(finalized.user_message, "continue");
        assert_eq!(finalized.runtime_required_texts.len(), 2);
        assert!(
            finalized.runtime_required_texts[0]
                .contains("Background task updates since your last turn:")
        );
        assert!(finalized.runtime_required_texts[0].contains("bg-shell-1 completed"));
        assert!(finalized.runtime_required_texts[0].contains("bg-shell-2 failed"));
        assert_eq!(
            finalized.runtime_required_texts[1],
            "Resume the interrupted task."
        );
        assert!(finalized.runtime_volatile_texts.is_empty());
        assert!(!finalized.user_message.contains("<system-reminder>"));
        assert!(state.pending_bg_notifications.is_empty());
        assert!(state.diagnostics_context.is_none());
    }

    #[tokio::test]
    async fn finalize_effective_line_injects_task_tool_reminder_and_resets_counter() {
        let mut state = SessionState {
            recent_tools: vec!["read_file".into(), "str_replace".into()],
            turns_since_task_use: 9,
            turns_since_task_reminder: 9,
            ..SessionState::default()
        };
        state.task_manager.rebind("sess-task-nudge");
        let create = state
            .task_manager
            .create(&serde_json::json!({"title": "Track the recovery cleanup"}))
            .await;
        assert!(!create.starts_with("Error:"), "{create}");
        let paused = state
            .task_manager
            .create(&serde_json::json!({"title": "Wait for operator input"}))
            .await;
        assert!(!paused.starts_with("Error:"), "{paused}");
        let pause_update = state
            .task_manager
            .update(&serde_json::json!({"task_id": "task-2", "new_status": "paused"}))
            .await;
        assert!(!pause_update.starts_with("Error:"), "{pause_update}");
        let done = state
            .task_manager
            .create(&serde_json::json!({"title": "Already finished"}))
            .await;
        assert!(!done.starts_with("Error:"), "{done}");
        let done_start = state
            .task_manager
            .update(&serde_json::json!({"task_id": "task-3", "new_status": "in_progress"}))
            .await;
        assert!(!done_start.starts_with("Error:"), "{done_start}");
        let done_update = state
            .task_manager
            .update(&serde_json::json!({"task_id": "task-3", "new_status": "completed"}))
            .await;
        assert!(!done_update.starts_with("Error:"), "{done_update}");
        let dependent = state
            .task_manager
            .create(&serde_json::json!({
                "title": "Ready after finished work",
                "add_blocked_by": ["task-3"]
            }))
            .await;
        assert!(!dependent.starts_with("Error:"), "{dependent}");

        let finalized =
            finalize_effective_line("continue".into(), "continue".into(), None, &mut state).await;

        assert_eq!(finalized.user_message, "continue");
        assert_eq!(finalized.runtime_volatile_texts.len(), 1);
        let snapshot = &finalized.runtime_volatile_texts[0];
        assert!(snapshot.contains("External task board snapshot:"));
        assert!(snapshot.contains("Track the recovery cleanup"));
        assert!(snapshot.contains("[paused] task-2: Wait for operator input"));
        assert!(snapshot.contains("Ready after finished work"), "{snapshot}");
        assert!(!snapshot.contains("blocked by: task-3"), "{snapshot}");
        assert!(
            !snapshot.contains("Already finished"),
            "terminal completed history should not clutter the open-work snapshot: {snapshot}"
        );
        assert!(
            !finalized
                .user_message
                .contains("External task board snapshot:")
        );
        assert_eq!(state.turns_since_task_use, 10);
        assert_eq!(state.turns_since_task_reminder, 0);
    }

    #[tokio::test]
    async fn finalize_effective_line_skips_task_tool_reminder_when_no_open_work() {
        let mut state = SessionState {
            recent_tools: vec!["read_file".into()],
            turns_since_task_use: 9,
            turns_since_task_reminder: 9,
            ..SessionState::default()
        };
        state.task_manager.rebind("sess-task-no-open-nudge");
        let done = state
            .task_manager
            .create(&serde_json::json!({"title": "Already finished"}))
            .await;
        assert!(!done.starts_with("Error:"), "{done}");
        let done_start = state
            .task_manager
            .update(&serde_json::json!({"task_id": "task-1", "new_status": "in_progress"}))
            .await;
        assert!(!done_start.starts_with("Error:"), "{done_start}");
        let done_update = state
            .task_manager
            .update(&serde_json::json!({"task_id": "task-1", "new_status": "completed"}))
            .await;
        assert!(!done_update.starts_with("Error:"), "{done_update}");

        let finalized =
            finalize_effective_line("continue".into(), "continue".into(), None, &mut state).await;

        assert!(
            finalized.runtime_volatile_texts.is_empty(),
            "no-open-work sessions should not get noisy task reminders: {finalized:?}"
        );
        assert!(
            !finalized
                .user_message
                .contains("External task board snapshot:"),
            "no-open-work sessions should not render an empty board reminder in user text: {finalized:?}"
        );
        assert_eq!(state.turns_since_task_use, 10);
        assert_eq!(
            state.turns_since_task_reminder, 0,
            "no-open-work checks should still be throttled"
        );
    }

    #[tokio::test]
    async fn finalize_effective_line_surfaces_task_reminder_load_failure_boundedly() {
        let mut state = SessionState {
            recent_tools: vec!["read_file".into()],
            turns_since_task_use: 9,
            turns_since_task_reminder: 9,
            ..SessionState::default()
        };
        state.task_manager = std::sync::Arc::new(TaskManager::new(
            "sess-task-load-fails",
            std::sync::Arc::new(FailingTaskLoadStore),
        ));

        let finalized =
            finalize_effective_line("continue".into(), "continue".into(), None, &mut state).await;

        assert_eq!(finalized.user_message, "continue");
        assert_eq!(finalized.runtime_volatile_texts.len(), 1);
        assert!(
            finalized.runtime_volatile_texts[0]
                .contains("External task board snapshot unavailable"),
            "task snapshot load failures must not be silently treated as no open work: {finalized:?}"
        );
        assert_eq!(state.turns_since_task_use, 10);
        assert_eq!(
            state.turns_since_task_reminder, 0,
            "load failure reminders should still be throttled"
        );
    }

    #[tokio::test]
    async fn finalize_effective_line_does_not_treat_non_task_tool_names_as_recent_use() {
        let mut state = SessionState {
            recent_tools: vec!["taskish".into()],
            turns_since_task_use: 9,
            turns_since_task_reminder: 9,
            ..SessionState::default()
        };
        state.task_manager.rebind("sess-task-non-task-nudge");
        let create = state
            .task_manager
            .create(&serde_json::json!({"title": "Open work"}))
            .await;
        assert!(!create.starts_with("Error:"), "{create}");

        let finalized =
            finalize_effective_line("continue".into(), "continue".into(), None, &mut state).await;

        assert_eq!(finalized.user_message, "continue");
        assert_eq!(finalized.runtime_volatile_texts.len(), 1);
        assert!(
            finalized.runtime_volatile_texts[0].contains("External task board snapshot:")
                && finalized.runtime_volatile_texts[0].contains("Open work"),
            "non-task tool names should not suppress the external task snapshot: {finalized:?}"
        );
        assert_eq!(state.turns_since_task_use, 10);
        assert_eq!(state.turns_since_task_reminder, 0);
    }
}
