use crate::{SessionState, StreamResult};
pub(crate) use astra_turn_core::followup_suggestion::FollowupSuggestion;

pub(crate) fn suggest_followup(
    line: &str,
    state: &SessionState,
    result: &StreamResult,
) -> Option<FollowupSuggestion> {
    let trimmed = line.trim();
    if trimmed.is_empty()
        || trimmed.starts_with('/')
        || astra_turn_core::chat_turn_heuristics::is_short_continuation_prompt(trimmed)
        || state.plan_mode_active()
        || state.executing_plan.is_some()
        || state.plan_handle.is_some()
        || state.pending_approval.is_some()
        || state.last_turn_interrupted
    {
        return None;
    }

    astra_turn_core::followup_suggestion::suggest_followup(
        trimmed,
        &result.full_text,
        &result.tools_used,
    )
}

#[cfg(test)]
mod tests {
    use super::suggest_followup;
    use crate::{SessionState, StreamResult};
    use astra_turn_core::followup_suggestion::FollowupSuggestionKind;

    fn base_state() -> SessionState {
        SessionState::default()
    }

    fn base_result(tools_used: Vec<&str>, full_text: &str) -> StreamResult {
        let tools_used: Vec<String> = tools_used.into_iter().map(str::to_string).collect();
        StreamResult {
            full_text: full_text.to_string(),
            tool_calls_count: tools_used.len() as u32,
            tools_used,
            ..Default::default()
        }
    }

    #[test]
    fn suggests_validation_after_edit_turn() {
        let suggestion = suggest_followup(
            "fix the bug",
            &base_state(),
            &base_result(vec!["str_replace"], "Fixed the bug."),
        )
        .expect("suggestion");
        assert_eq!(suggestion.text, "run the tests");
        assert_eq!(suggestion.kind, FollowupSuggestionKind::Validate);
    }

    #[test]
    fn suggests_chinese_validation_after_edit_turn() {
        let suggestion = suggest_followup(
            "修一下这个 bug",
            &base_state(),
            &base_result(vec!["str_replace"], "已经修好了。"),
        )
        .expect("suggestion");
        assert_eq!(suggestion.text, "跑一下测试");
        assert_eq!(suggestion.kind, FollowupSuggestionKind::Validate);
    }

    #[test]
    fn suggests_commit_after_validated_edit_turn() {
        let suggestion = suggest_followup(
            "fix the bug",
            &base_state(),
            &base_result(
                vec!["str_replace", "run_build_test"],
                "Patched and verified.",
            ),
        )
        .expect("suggestion");
        assert_eq!(suggestion.text, "commit this");
        assert_eq!(suggestion.kind, FollowupSuggestionKind::Commit);
    }

    #[test]
    fn suggests_push_after_commit_turn() {
        let suggestion = suggest_followup(
            "commit it",
            &base_state(),
            &base_result(vec!["git_commit"], "Committed the changes."),
        )
        .expect("suggestion");
        assert_eq!(suggestion.text, "push it");
        assert_eq!(suggestion.kind, FollowupSuggestionKind::Push);
    }

    #[test]
    fn suppresses_suggestion_when_assistant_is_asking() {
        assert_eq!(
            suggest_followup(
                "fix the bug",
                &base_state(),
                &base_result(
                    vec!["str_replace"],
                    "I have two valid options. Which one do you want me to try?"
                ),
            ),
            None
        );
    }

    #[test]
    fn suggests_continue_when_assistant_asks_to_continue() {
        let suggestion = suggest_followup(
            "修一下这个 bug",
            &base_state(),
            &base_result(Vec::new(), "已经定位到原因了，要我继续改吗？"),
        )
        .expect("suggestion");
        assert_eq!(suggestion.text, "继续");
        assert_eq!(suggestion.kind, FollowupSuggestionKind::Continue);
    }

    #[test]
    fn suggests_commit_when_assistant_asks_about_commit() {
        let suggestion = suggest_followup(
            "修一下这个 bug",
            &base_state(),
            &base_result(
                vec!["str_replace", "run_build_test"],
                "已经修好并验证了，要我直接提交吗？",
            ),
        )
        .expect("suggestion");
        assert_eq!(suggestion.text, "提交一下");
        assert_eq!(suggestion.kind, FollowupSuggestionKind::Commit);
    }

    #[test]
    fn suppresses_suggestion_for_short_continuation_turns() {
        assert_eq!(
            suggest_followup(
                "继续",
                &base_state(),
                &base_result(vec!["str_replace"], "Patched the file."),
            ),
            None
        );
    }
}
