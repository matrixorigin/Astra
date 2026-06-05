use super::*;
use astra_turn_core::chat_turn_heuristics::is_short_continuation_prompt;

/// Correction phrase patterns that indicate user is redirecting/correcting.
const CORRECTION_PATTERNS: &[&str] = &[
    "no,",
    "no i",
    "that's wrong",
    "that's not",
    "i meant",
    "i mean",
    "not that",
    "wrong,",
    "wrong.",
    "incorrect",
    "actually,",
    "actually i",
    "instead,",
    "forget that",
    "ignore that",
    "let me clarify",
    "to clarify",
    "what i want",
    "wait,",
    "hold on",
    "stop,",
    "不对",
    "错了",
    "不是这样",
    "我的意思是",
    "我是说",
    "等等",
    "停一下",
];

/// Detect if a user message appears to be a correction/redirection.
pub(crate) fn detect_correction_signal(message: &str) -> bool {
    let lower = message.to_lowercase();
    CORRECTION_PATTERNS
        .iter()
        .any(|pattern| lower.contains(pattern))
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
            "<system-reminder>\nBackground task updates since your last turn:\n{notifications}\n</system-reminder>\n\n{effective_line}"
        );
    }

    const TURNS_SINCE_TASK_USE_THRESHOLD: u32 = 10;
    const TURNS_BETWEEN_REMINDERS: u32 = 10;
    if state
        .recent_tools
        .iter()
        .any(|tool| tool == "task" || tool.starts_with("task_"))
    {
        state.turns_since_task_use = 0;
    } else {
        state.turns_since_task_use += 1;
    }
    state.turns_since_task_reminder += 1;

    if state.turns_since_task_use >= TURNS_SINCE_TASK_USE_THRESHOLD
        && state.turns_since_task_reminder >= TURNS_BETWEEN_REMINDERS
    {
        let task_list = state
            .task_manager
            .list(&serde_json::json!({"status_filter": "active"}))
            .await;
        let nudge = format!(
            "<system-reminder>\n\
            The task tools haven't been used recently. If you're working on tasks that would benefit from tracking progress, \
            consider using task(action='create') to add new tasks and task(action='update') to update task status \
            (set to in_progress when starting, completed when done). Also consider cleaning up the task list if it has become stale. \
            Only use these if relevant to the current work. This is just a gentle reminder - ignore if not applicable. \
            Make sure that you NEVER mention this reminder to the user\n\
            \n\
            Here are the existing tasks:\n{task_list}\n\
            </system-reminder>"
        );
        effective_line = format!("{nudge}\n\n{effective_line}");
        state.turns_since_task_reminder = 0;
    }

    apply_resume_context(effective_line, resume_guidance)
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
        let block = crate::format_project_instructions(project_instructions);
        effective_line = format!("{block}\n\n{effective_line}");
    }

    if let Some(anchor) = state
        .continuation_anchor
        .as_deref()
        .filter(|_| is_low_information_followup(line))
    {
        let task_board_reanchor = if anchor.contains("Active task board:") {
            "If the active thread already has a task board, reconcile it before proceeding: create any missing tasks implied by the approved plan, set the current task to in_progress before doing the work, and update statuses as tasks complete.\n"
        } else {
            ""
        };
        effective_line = format!(
            "[Active task attachment]\n\
Resume the active task/thread below unless the user explicitly changes topic.\n\
Treat brief follow-ups as actions on this active thread, not as brand-new unrelated tasks.\n\
If the follow-up asks to fix / patch / test / continue, apply that action to this active thread.\n\
{task_board_reanchor}\
{anchor}\n\n[User follow-up]\n{effective_line}"
        );
    }

    effective_line
}

fn contains_any_token(haystack: &str, tokens: &[&str]) -> bool {
    tokens.iter().any(|token| haystack.contains(token))
}

pub(crate) fn is_low_information_followup(line: &str) -> bool {
    if is_short_continuation_prompt(line) {
        return true;
    }

    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.chars().count() > 32 {
        return false;
    }

    let lower = trimmed.to_ascii_lowercase();
    let has_action = contains_any_token(
        &lower,
        &[
            "fix",
            "patch",
            "repair",
            "implement",
            "apply",
            "edit",
            "update",
            "test",
            "verify",
            "run",
            "commit",
            "push",
            "continue",
            "resume",
            "retry",
        ],
    ) || contains_any_token(
        trimmed,
        &[
            "修复",
            "修一下",
            "改一下",
            "改下",
            "处理一下",
            "处理下",
            "优化一下",
            "优化下",
            "测一下",
            "测试一下",
            "验证一下",
            "提交一下",
            "推一下",
            "继续",
            "重试",
        ],
    );
    if !has_action {
        return false;
    }

    let has_deictic_reference =
        contains_any_token(&lower, &["this", "it", "that", "them", "here", "there"])
            || contains_any_token(trimmed, &["这", "这个", "这里", "它", "这些", "那个"]);
    let has_question_shape =
        trimmed.ends_with('?') || trimmed.ends_with('？') || trimmed.ends_with('吗');
    let token_count = trimmed
        .split(|c: char| c.is_whitespace() || c == ',' || c == '，')
        .filter(|part| !part.is_empty())
        .count();
    let short_ascii_action =
        (trimmed.is_ascii() || trimmed.contains(char::is_whitespace)) && token_count <= 3;

    has_deictic_reference || has_question_shape || short_ascii_action
}

#[cfg(test)]
mod tests {
    use super::*;

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
                    .to_string(),
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
                    .to_string(),
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
                "Latest user task: improve session memory flow\nActive task board:\n- [in_progress] task-1: Phase 1: /memory show — TDD"
                    .to_string(),
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
        assert!(effective.contains("Active task board:"));
        assert!(effective.contains("[User follow-up]\n还有什么？"));
    }

    #[test]
    fn build_effective_line_leaves_normal_prompt_untouched() {
        let state = SessionState {
            continuation_anchor: Some("Latest user task: debug Chinese input drops".to_string()),
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
            skill_dev: Some(crate::cli::SkillDevState {
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
            skill_dev: Some(crate::cli::SkillDevState {
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
            skill_dev: Some(crate::cli::SkillDevState {
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
            skill_dev: Some(crate::cli::SkillDevState {
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
            skill_dev: Some(crate::cli::SkillDevState {
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
            skill_dev: Some(crate::cli::SkillDevState {
                name: "combo".to_string(),
                dir: skill_dir,
            }),
            active_system_skills: vec![prompts::builtin_concise_skill()],
            continuation_anchor: Some("Previous task: fix auth".to_string()),
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
        assert!(finalized.contains("Background task updates since your last turn:"));
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

        let finalized = finalize_effective_line("continue".into(), None, &mut state).await;

        assert!(finalized.contains("The task tools haven't been used recently."));
        assert!(finalized.contains("Track the recovery cleanup"));
        assert_eq!(state.turns_since_task_use, 10);
        assert_eq!(state.turns_since_task_reminder, 0);
    }
}
