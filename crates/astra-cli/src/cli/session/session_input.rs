use crate::cli::project_instructions::format_project_instructions;
use crate::cli::session::session_state::SessionState;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FinalizedInput {
    pub(crate) user_message: String,
    pub(crate) user_intent: String,
    /// External/session-recovery context required for the next turn.
    pub(crate) runtime_required_texts: Vec<String>,
    /// Dynamic text from external session sources. Internal runtime state
    /// uses required/typed lanes and must not be projected here.
    pub(crate) runtime_volatile_texts: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PreparedInput {
    pub(crate) user_message: String,
    pub(crate) runtime_required_texts: Vec<String>,
}

impl PreparedInput {
    pub(crate) fn user_only(user_message: impl Into<String>) -> Self {
        Self {
            user_message: user_message.into(),
            runtime_required_texts: Vec::new(),
        }
    }
}

pub(crate) fn clear_pending_recovery_for_ordinary_chat_input(state: &mut SessionState) {
    state.pending_recovery = None;
    state.resume_restricted_tools.clear();
}

pub(crate) fn finalize_effective_line(
    prepared: PreparedInput,
    user_intent: String,
    state: &mut SessionState,
) -> FinalizedInput {
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

    if let Some(guidance) = state.resume_guidance.as_ref()
        && !guidance.trim().is_empty()
    {
        runtime_required_texts.push(guidance.clone());
    }

    FinalizedInput {
        user_message: prepared.user_message,
        user_intent,
        runtime_required_texts,
        runtime_volatile_texts,
    }
}

pub(crate) fn prepare_input(line: &str, state: &SessionState) -> PreparedInput {
    let mut runtime_required_texts = Vec::new();

    if let Some(project_instructions) = state.project_instructions.as_ref() {
        runtime_required_texts.push(format_project_instructions(project_instructions));
    }

    if let Some(diagnostics_context) = state.diagnostics_context.as_ref() {
        runtime_required_texts.push(diagnostics_context.clone());
    }

    PreparedInput {
        user_message: line.to_string(),
        runtime_required_texts,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        PreparedInput, clear_pending_recovery_for_ordinary_chat_input, finalize_effective_line,
        prepare_input,
    };
    use crate::cli::session::session_state::{ContinuationAnchor, SessionState};

    #[test]
    fn build_effective_line_does_not_phrase_match_short_continue() {
        let state = SessionState {
            continuation_anchor: Some(ContinuationAnchor::rendered_for_test(
                "Latest user input: debug Chinese input drops\nLatest assistant direction: inspect prompt redraw path"
                    .to_string(),
            )),
            ..SessionState::default()
        };

        let prepared = prepare_input("继续", &state);
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

        let prepared = prepare_input("修复?", &state);
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

        let prepared = prepare_input("修一下输入法问题", &state);
        assert_eq!(prepared.user_message, "修一下输入法问题");
        assert!(prepared.runtime_required_texts.is_empty());
    }

    #[test]
    fn finalize_effective_line_routes_resume_guidance_to_required_lane() {
        let mut state = SessionState {
            resume_guidance: Some("Resume the interrupted turn before answering.".into()),
            ..SessionState::default()
        };

        let finalized = finalize_effective_line(
            PreparedInput::user_only("continue"),
            "raw continue".into(),
            &mut state,
        );

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
    fn explain_artifact_context_is_not_injected_from_a_client_local_store() {
        let session_id = "9a5c2f6e-0f88-44db-a7a4-5e89c1d2f304";
        let state = SessionState {
            session_id: Some(session_id.to_string()),
            ..SessionState::default()
        };
        let prepared = prepare_input("analyze the previous explain", &state);
        assert!(prepared.runtime_required_texts.is_empty());
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

    #[test]
    fn finalize_effective_line_drains_notifications_without_mutating_user_message() {
        let mut state = SessionState {
            diagnostics_context: Some("<diag/>".into()),
            resume_guidance: Some("Resume the interrupted task.".into()),
            pending_bg_notifications: vec![
                "bg-shell-1 completed".into(),
                "bg-shell-2 failed".into(),
            ],
            ..SessionState::default()
        };

        let finalized = finalize_effective_line(
            PreparedInput::user_only("continue"),
            "continue".into(),
            &mut state,
        );

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
        assert_eq!(state.diagnostics_context.as_deref(), Some("<diag/>"));
    }

    #[tokio::test]
    async fn finalize_effective_line_does_not_parse_ui_task_projection_as_runtime_truth() {
        let mut state = SessionState::default();
        *state.bg_task_list_cache.write().await = r#"<background_tasks count="1"><task id="fanout:review-group" kind="agent_fanout" status="running" completed="1" active="2" recovery_call="agent_fanout(action='get_results', group_id='review-group')" /></background_tasks>"#.into();

        let finalized = finalize_effective_line(
            PreparedInput::user_only("what is running?"),
            "what is running?".into(),
            &mut state,
        );

        assert!(
            finalized.runtime_volatile_texts.is_empty(),
            "rendered UI state must not become a parallel model truth lane: {finalized:?}"
        );
        assert!(!finalized.user_message.contains("background_tasks"));
    }
}
