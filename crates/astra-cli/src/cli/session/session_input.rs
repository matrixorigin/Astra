use crate::cli::project_instructions::format_project_instructions;
use crate::cli::session::session_state::SessionState;
use astra_runtime::prompts;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FinalizedInput {
    pub(crate) user_message: String,
    pub(crate) user_intent: String,
    /// External/session-recovery context required for the next turn.
    pub(crate) runtime_required_texts: Vec<String>,
    /// Dynamic text from external session sources. Internal runtime state
    /// uses required/typed lanes and must not be projected here.
    pub(crate) runtime_volatile_texts: Vec<String>,
    /// Producer-owned names for built-in system skills active on this turn.
    /// The payload builder projects these into `edge_profile.active_skills`;
    /// it must never rediscover them by parsing prompt text.
    pub(crate) active_system_skill_names: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PreparedInput {
    pub(crate) user_message: String,
    pub(crate) runtime_required_texts: Vec<String>,
    pub(crate) active_system_skill_names: Vec<String>,
}

impl PreparedInput {
    pub(crate) fn user_only(user_message: impl Into<String>) -> Self {
        Self {
            user_message: user_message.into(),
            runtime_required_texts: Vec::new(),
            active_system_skill_names: Vec::new(),
        }
    }
}

pub(crate) fn clear_pending_recovery_for_ordinary_chat_input(state: &mut SessionState) {
    state.pending_recovery = None;
    state.resume_restricted_tools.clear();
}

pub(crate) async fn finalize_effective_line(
    prepared: PreparedInput,
    user_intent: String,
    resume_guidance: Option<String>,
    state: &mut SessionState,
) -> FinalizedInput {
    state.diagnostics_context = None;
    let mut runtime_required_texts = prepared.runtime_required_texts;
    let runtime_volatile_texts = Vec::new();

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

    if let Some(guidance) = resume_guidance
        && !guidance.trim().is_empty()
    {
        runtime_required_texts.push(guidance);
    }

    FinalizedInput {
        user_message: prepared.user_message,
        user_intent,
        runtime_required_texts,
        runtime_volatile_texts,
        active_system_skill_names: prepared.active_system_skill_names,
    }
}

pub(crate) fn prepare_input(
    line: &str,
    state: &SessionState,
    ui: &mut dyn crate::cli::ui_adapter::ReplUiAdapter,
) -> PreparedInput {
    let mut runtime_required_texts = Vec::new();

    if let Some(project_instructions) = state.project_instructions.as_ref() {
        runtime_required_texts.push(format_project_instructions(project_instructions));
    }

    if let Some(diagnostics_context) = state.diagnostics_context.as_ref() {
        runtime_required_texts.push(diagnostics_context.clone());
    }

    if !state.active_system_skills.is_empty() {
        runtime_required_texts.push(prompts::build_skill_instructions(
            &state.active_system_skills,
        ));
    }

    if let Some(skill_dev) = state.skill_dev.as_ref() {
        let skill_md = skill_dev.dir.join("SKILL.md");
        match std::fs::read_to_string(&skill_md) {
            Ok(source) if !source.trim().is_empty() => {
                runtime_required_texts.push(prompts::build_skill_dev_context(
                    &skill_dev.name,
                    &skill_md.display().to_string(),
                    &source,
                ));
            }
            Ok(_) => {
                ui.show_warning(&format!(
                    "  ⚠ SKILL.md is empty at {}, dev context skipped",
                    skill_md.display()
                ));
            }
            Err(_) => {
                ui.show_warning(&format!(
                    "  ⚠ SKILL.md not found at {}, dev context skipped",
                    skill_md.display()
                ));
            }
        }
    }

    PreparedInput {
        user_message: line.to_string(),
        runtime_required_texts,
        active_system_skill_names: state
            .active_system_skills
            .iter()
            .map(|skill| skill.name.clone())
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        PreparedInput, clear_pending_recovery_for_ordinary_chat_input, finalize_effective_line,
        prepare_input,
    };
    use crate::cli::session::session_state::{ContinuationAnchor, SessionState, SkillDevState};
    use astra_runtime::prompts;

    #[test]
    fn build_effective_line_does_not_phrase_match_short_continue() {
        let state = SessionState {
            continuation_anchor: Some(ContinuationAnchor::rendered_for_test(
                "Latest user input: debug Chinese input drops\nLatest assistant direction: inspect prompt redraw path"
                    .to_string(),
            )),
            ..SessionState::default()
        };

        let prepared = prepare_input("继续", &state, &mut crate::cli::ui_adapter::LineUiAdapter);
        assert!(prepared.runtime_required_texts.is_empty());
        assert_eq!(prepared.user_message, "继续");
    }

    #[test]
    fn build_effective_line_does_not_reanchor_repair_followup_by_phrase() {
        let state = SessionState {
            continuation_anchor: Some(ContinuationAnchor::rendered_for_test(
                "Latest user input: review commit aa1f419b\nLatest assistant summary:\n## Review\nP5 still blocks large merges",
            )),
            ..SessionState::default()
        };

        let prepared = prepare_input("修复?", &state, &mut crate::cli::ui_adapter::LineUiAdapter);
        assert_eq!(prepared.user_message, "修复?");
        assert!(prepared.runtime_required_texts.is_empty());
    }

    #[test]
    fn build_effective_line_leaves_normal_prompt_untouched() {
        let state = SessionState {
            continuation_anchor: Some(ContinuationAnchor::rendered_for_test(
                "Latest user input: debug Chinese input drops",
            )),
            ..SessionState::default()
        };

        let prepared = prepare_input(
            "修一下输入法问题",
            &state,
            &mut crate::cli::ui_adapter::LineUiAdapter,
        );
        assert_eq!(prepared.user_message, "修一下输入法问题");
        assert!(prepared.runtime_required_texts.is_empty());
    }

    #[tokio::test]
    async fn finalize_effective_line_routes_resume_guidance_to_required_lane() {
        let mut state = SessionState::default();

        let finalized = finalize_effective_line(
            PreparedInput::user_only("continue"),
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

        let prepared = prepare_input(
            "improve this skill",
            &state,
            &mut crate::cli::ui_adapter::LineUiAdapter,
        );
        assert_eq!(prepared.user_message, "improve this skill");
        assert_eq!(prepared.runtime_required_texts.len(), 1);
        assert!(prepared.runtime_required_texts[0].contains("[SKILL DEV: test-skill]"));
        assert!(prepared.runtime_required_texts[0].contains("Do stuff."));
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

        let turn1 = prepare_input("check", &state, &mut crate::cli::ui_adapter::LineUiAdapter);
        assert!(turn1.runtime_required_texts[0].contains(OLD_BODY));

        std::fs::write(
            skill_dir.join("SKILL.md"),
            format!("---\nname: evolving\n---\n{NEW_BODY}"),
        )
        .unwrap();

        let turn2 = prepare_input(
            "check again",
            &state,
            &mut crate::cli::ui_adapter::LineUiAdapter,
        );
        assert!(
            !turn2.runtime_required_texts[0].contains(OLD_BODY),
            "should not contain old skill body"
        );
        assert!(
            turn2.runtime_required_texts[0].contains(NEW_BODY),
            "should contain new content"
        );
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

        let prepared = prepare_input("hello", &state, &mut crate::cli::ui_adapter::LineUiAdapter);
        assert_eq!(prepared, PreparedInput::user_only("hello"));
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

        let prepared = prepare_input("hello", &state, &mut crate::cli::ui_adapter::LineUiAdapter);
        assert_eq!(prepared, PreparedInput::user_only("hello"));
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

        let prepared = prepare_input("x", &state, &mut crate::cli::ui_adapter::LineUiAdapter);
        let expected_path = skill_dir.join("SKILL.md").display().to_string();
        assert!(
            prepared.runtime_required_texts[0].contains(&expected_path),
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
            continuation_anchor: Some(ContinuationAnchor::rendered_for_test(
                "Previous task: fix auth",
            )),
            ..SessionState::default()
        };

        let prepared = prepare_input(
            "continue",
            &state,
            &mut crate::cli::ui_adapter::LineUiAdapter,
        );
        assert_eq!(prepared.user_message, "continue");
        assert_eq!(prepared.active_system_skill_names, vec!["concise"]);
        assert_eq!(prepared.runtime_required_texts.len(), 2);
        assert!(prepared.runtime_required_texts[0].contains("Concise"));
        assert!(prepared.runtime_required_texts[1].contains("[SKILL DEV: combo]"));
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
            PreparedInput::user_only("continue"),
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
    async fn finalize_effective_line_does_not_parse_ui_task_projection_as_runtime_truth() {
        let mut state = SessionState::default();
        *state.bg_task_list_cache.write().await = r#"<background_tasks count="1"><task id="fanout:review-group" kind="agent_fanout" status="running" completed="1" active="2" recovery_call="agent_fanout(action='get_results', group_id='review-group')" /></background_tasks>"#.into();

        let finalized = finalize_effective_line(
            PreparedInput::user_only("what is running?"),
            "what is running?".into(),
            None,
            &mut state,
        )
        .await;

        assert!(
            finalized.runtime_volatile_texts.is_empty(),
            "rendered UI state must not become a parallel model truth lane: {finalized:?}"
        );
        assert!(!finalized.user_message.contains("background_tasks"));
    }
}
