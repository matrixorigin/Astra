use crate::cli::project_instructions::format_project_instructions;
use crate::cli::session::session_state::{ContinuationAnchor, SessionState};
use astra_runtime::prompts;
use astra_tools::task_mgmt::SessionTask;
use astra_turn_core::input_classifier;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ActiveTaskAttachment {
    pub anchor: ContinuationAnchor,
    pub followup: String,
}

impl ActiveTaskAttachment {
    pub(crate) fn render(&self, effective_line: &str) -> String {
        let task_board_reanchor = if self.anchor.has_active_task_board() {
            "If the active thread already has a task board, reconcile it before proceeding: create any missing tasks implied by the approved plan, call task(action='update', task_id='...', new_status='in_progress') before doing the work, and update with new_status='completed' as tasks complete.\n"
        } else {
            ""
        };
        format!(
            "[Active task attachment]\n\
Resume the active task/thread below unless the user explicitly changes topic.\n\
Treat brief follow-ups as actions on this active thread, not as brand-new unrelated tasks.\n\
If the follow-up asks to fix / patch / test / continue, apply that action to this active thread.\n\
{task_board_reanchor}\
{}\n\n[User follow-up]\n{effective_line}",
            self.anchor.text
        )
    }

    pub(crate) fn semantic_query(&self) -> String {
        let mut parts = Vec::new();
        if let Some(task) = self.anchor.latest_user_task.as_deref() {
            parts.push(format!("Task: {task}"));
        }
        if !self.anchor.active_task_board.is_empty() {
            parts.push(format!(
                "Open tasks: {}",
                self.anchor.active_task_board.join(" | ")
            ));
        }
        if let Some(direction) = self.anchor.assistant_direction.as_deref() {
            parts.push(format!("Assistant summary: {direction}"));
        }
        parts.push(format!("Follow-up: {}", self.followup.trim()));
        parts.join("\n")
    }
}

/// Detect if a user message appears to be a correction/redirection.
pub(crate) fn detect_correction_signal(message: &str) -> bool {
    input_classifier::is_correction_signal(message)
}

pub(crate) fn apply_resume_context(
    mut effective_line: String,
    resume_guidance: Option<String>,
) -> String {
    if let Some(guidance) = resume_guidance {
        effective_line = format!("{guidance}\n\n{effective_line}");
    }
    effective_line
}

pub(crate) fn clear_pending_recovery_for_ordinary_chat_input(state: &mut SessionState) {
    state.pending_recovery = None;
    state.resume_restricted_tools.clear();
}

pub(crate) async fn finalize_effective_line(
    mut effective_line: String,
    resume_guidance: Option<String>,
    state: &mut SessionState,
) -> String {
    state.diagnostics_context = None;

    if !state.pending_bg_notifications.is_empty() {
        let notifications = state
            .pending_bg_notifications
            .drain(..)
            .collect::<Vec<_>>()
            .join("\n");
        effective_line = format!(
            "<system-reminder>\nBackground command updates since your last turn:\n{notifications}\n</system-reminder>\n\n{effective_line}"
        );
    }

    const TURNS_SINCE_TASK_USE_THRESHOLD: u32 = 10;
    const TURNS_BETWEEN_REMINDERS: u32 = 10;
    if state.recent_tools.iter().any(|tool| tool == "task") {
        state.turns_since_task_use = 0;
    } else {
        state.turns_since_task_use += 1;
    }
    state.turns_since_task_reminder += 1;

    if state.turns_since_task_use >= TURNS_SINCE_TASK_USE_THRESHOLD
        && state.turns_since_task_reminder >= TURNS_BETWEEN_REMINDERS
    {
        let nudge = match state.task_manager.load_active_tasks().await {
            Ok(tasks) => format_open_task_reminder_list(&tasks).map(|task_list| {
                format!(
                    "<system-reminder>\n\
                The task tools haven't been used recently. If you're working on tasks that would benefit from tracking progress, \
                consider using task(action='create') to add new tasks and task(action='update', task_id='...', new_status='...') to update task status \
                (set new_status='in_progress' when starting, new_status='completed' when done, or new_status='paused' when waiting). Also consider cleaning up the task list if it has become stale. \
                Only use these if relevant to the current work. This is just a gentle reminder - ignore if not applicable. \
                Make sure that you NEVER mention this reminder to the user\n\
                \n\
                Here is the open task board:\n{task_list}\n\
                </system-reminder>"
                )
            }),
            Err(error) => Some(format!(
                "<system-reminder>\n\
                Task board state could not be loaded while checking whether a task reminder is needed: {error}\n\
                Do not assume there are no open tasks. If task tracking is relevant to the current work, retry task(action='list') or continue without task updates if unrelated. \
                This reminder is throttled; never mention it to the user.\n\
                </system-reminder>"
            )),
        };
        if let Some(nudge) = nudge {
            effective_line = format!("{nudge}\n\n{effective_line}");
        }
        state.turns_since_task_reminder = 0;
    }

    apply_resume_context(effective_line, resume_guidance)
}

fn format_open_task_reminder_list(tasks: &[SessionTask]) -> Option<String> {
    let mut lines: Vec<String> = tasks
        .iter()
        .filter(|task| task.status.is_open_work())
        .take(10)
        .map(|task| {
            let title = task.title.chars().take(120).collect::<String>();
            format!("- [{}] {}: {}", task.status, task.id, title)
        })
        .collect();
    let open_total = tasks
        .iter()
        .filter(|task| task.status.is_open_work())
        .count();
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

pub(crate) fn active_task_attachment(
    line: &str,
    state: &SessionState,
) -> Option<ActiveTaskAttachment> {
    let anchor = state
        .continuation_anchor
        .clone()
        .filter(|_| is_low_information_followup(line))?;
    Some(ActiveTaskAttachment {
        anchor,
        followup: line.to_string(),
    })
}

pub(crate) fn build_effective_line_with_attachment(
    line: &str,
    state: &SessionState,
    ui: &mut dyn crate::cli::ui_adapter::ReplUiAdapter,
    attachment: Option<&ActiveTaskAttachment>,
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

    if let Some(attachment) = attachment {
        effective_line = attachment.render(&effective_line);
    }

    effective_line
}

pub(crate) fn build_effective_line(
    line: &str,
    state: &SessionState,
    ui: &mut dyn crate::cli::ui_adapter::ReplUiAdapter,
) -> String {
    let attachment = active_task_attachment(line, state);
    build_effective_line_with_attachment(line, state, ui, attachment.as_ref())
}

pub(crate) fn is_low_information_followup(line: &str) -> bool {
    input_classifier::is_low_information_followup(line)
}

#[cfg(test)]
mod tests {
    use super::{
        apply_resume_context, build_effective_line, clear_pending_recovery_for_ordinary_chat_input,
        detect_correction_signal, finalize_effective_line, is_low_information_followup,
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
    fn build_effective_line_injects_anchor_for_short_continue() {
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
        assert!(effective.contains("[Active task attachment]"));
        assert!(effective.contains("debug Chinese input drops"));
        assert!(effective.contains("[User follow-up]\n继续"));
    }

    #[test]
    fn low_information_followup_detects_repair_prompts() {
        assert!(is_low_information_followup("修复?"));
        assert!(is_low_information_followup("fix this"));
        assert!(is_low_information_followup("test it"));
        assert!(is_low_information_followup("还有什么？"));
        assert!(!is_low_information_followup("修一下输入法问题"));
        assert!(!is_low_information_followup(
            "implement request batching in runtime selector"
        ));
    }

    #[test]
    fn build_effective_line_injects_attachment_for_low_information_repair_followup() {
        let state = SessionState {
            continuation_anchor: Some(
                "Latest user task: review commit aa1f419b\nLatest assistant summary:\n## Review\nP5 still blocks large merges"
                    .into(),
            ),
            ..SessionState::default()
        };

        let effective =
            build_effective_line("修复?", &state, &mut crate::cli::ui_adapter::LineUiAdapter);
        assert!(effective.contains("[Active task attachment]"));
        assert!(effective.contains("review commit aa1f419b"));
        assert!(effective.contains("fix / patch / test / continue"));
        assert!(effective.contains("[User follow-up]\n修复?"));
    }

    #[test]
    fn build_effective_line_reanchors_generic_followup_to_task_board() {
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
        assert!(effective.contains("[Active task attachment]"));
        assert!(effective.contains("reconcile it before proceeding"));
        assert!(effective.contains("new_status='in_progress'"));
        assert!(effective.contains("new_status='completed'"));
        assert!(effective.contains("Active task board:"));
        assert!(effective.contains("[User follow-up]\n还有什么？"));
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

    #[test]
    fn apply_resume_context_prepends_resume_guidance() {
        let effective = apply_resume_context(
            "continue".to_string(),
            Some("Resume the interrupted turn before answering.".to_string()),
        );
        assert!(effective.starts_with("Resume the interrupted turn before answering."));
        assert!(effective.ends_with("\n\ncontinue"));
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
        assert!(effective.contains("[Active task attachment]"), "anchor");
        assert!(effective.contains("fix auth"), "anchor content");
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
    async fn finalize_effective_line_drains_notifications_and_prepends_resume_guidance() {
        let mut state = SessionState {
            diagnostics_context: Some("<diag/>".into()),
            pending_bg_notifications: vec!["job-1 done".into(), "job-2 failed".into()],
            ..SessionState::default()
        };

        let finalized = finalize_effective_line(
            "continue".into(),
            Some("Resume the interrupted task.".into()),
            &mut state,
        )
        .await;

        assert!(finalized.starts_with("Resume the interrupted task."));
        assert!(finalized.contains("Background command updates since your last turn:"));
        assert!(finalized.contains("job-1 done"));
        assert!(finalized.contains("job-2 failed"));
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
        let done_update = state
            .task_manager
            .update(&serde_json::json!({"task_id": "task-3", "new_status": "completed"}))
            .await;
        assert!(!done_update.starts_with("Error:"), "{done_update}");

        let finalized = finalize_effective_line("continue".into(), None, &mut state).await;

        assert!(finalized.contains("The task tools haven't been used recently."));
        assert!(finalized.contains("new_status='in_progress'"));
        assert!(finalized.contains("new_status='completed'"));
        assert!(finalized.contains("new_status='paused'"));
        assert!(finalized.contains("Here is the open task board:"));
        assert!(finalized.contains("Track the recovery cleanup"));
        assert!(finalized.contains("[paused] task-2: Wait for operator input"));
        assert!(
            !finalized.contains("Already finished"),
            "terminal completed history should not clutter the open-work reminder: {finalized}"
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
        let done_update = state
            .task_manager
            .update(&serde_json::json!({"task_id": "task-1", "new_status": "completed"}))
            .await;
        assert!(!done_update.starts_with("Error:"), "{done_update}");

        let finalized = finalize_effective_line("continue".into(), None, &mut state).await;

        assert!(
            !finalized.contains("The task tools haven't been used recently."),
            "no-open-work sessions should not get noisy task reminders: {finalized}"
        );
        assert!(
            !finalized.contains("Here is the open task board:"),
            "no-open-work sessions should not render an empty board reminder: {finalized}"
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

        let finalized = finalize_effective_line("continue".into(), None, &mut state).await;

        assert!(
            finalized.contains("Task board state could not be loaded"),
            "task reminder load failures must not be silently treated as no open work: {finalized}"
        );
        assert!(
            finalized.contains("Do not assume there are no open tasks"),
            "load failure reminder should preserve the task-board uncertainty: {finalized}"
        );
        assert_eq!(state.turns_since_task_use, 10);
        assert_eq!(
            state.turns_since_task_reminder, 0,
            "load failure reminders should still be throttled"
        );
    }

    #[tokio::test]
    async fn finalize_effective_line_does_not_treat_legacy_task_tool_names_as_recent_use() {
        let mut state = SessionState {
            recent_tools: vec!["task_create".into()],
            turns_since_task_use: 9,
            turns_since_task_reminder: 9,
            ..SessionState::default()
        };
        state.task_manager.rebind("sess-task-legacy-nudge");
        let create = state
            .task_manager
            .create(&serde_json::json!({"title": "Open work"}))
            .await;
        assert!(!create.starts_with("Error:"), "{create}");

        let finalized = finalize_effective_line("continue".into(), None, &mut state).await;

        assert!(
            finalized.contains("The task tools haven't been used recently.")
                && finalized.contains("Open work"),
            "legacy task_* tool names should not suppress the canonical task reminder: {finalized}"
        );
        assert_eq!(state.turns_since_task_use, 10);
        assert_eq!(state.turns_since_task_reminder, 0);
    }
}
