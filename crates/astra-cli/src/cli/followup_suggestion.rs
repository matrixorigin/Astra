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
        || state.plan_mode_active()
        || state.executing_plan.is_some()
        || state.plan_handle.is_some()
        || state.pending_approval.is_some()
        || state.last_turn_interrupted
    {
        return None;
    }

    let markers = if result.tool_call_records.is_empty() {
        result.tools_used.clone()
    } else {
        result
            .tool_call_records
            .iter()
            .map(|record| {
                astra_turn_core::followup_suggestion::tool_marker(
                    &record.name,
                    record.args_full.as_deref(),
                )
            })
            .collect()
    };

    astra_turn_core::followup_suggestion::suggest_followup(trimmed, &result.full_text, &markers)
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

    fn result_with_git_action_commit_record(full_text: &str) -> StreamResult {
        StreamResult {
            full_text: full_text.to_string(),
            tool_calls_count: 1,
            tools_used: vec!["git".to_string()],
            tool_call_records: vec![astra_services::session_journal::ToolCallRecord {
                name: "git".to_string(),
                ok: true,
                args_full: Some(r#"{"action":"commit","message":"ship"}"#.to_string()),
                ..Default::default()
            }],
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
            &result_with_git_action_commit_record("Committed the changes."),
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
    fn does_not_suggest_continue_from_phrase_match() {
        let suggestion = suggest_followup(
            "修一下这个 bug",
            &base_state(),
            &base_result(Vec::new(), "已经定位到原因了，要我继续改吗？"),
        );
        assert_eq!(suggestion, None);
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
    fn short_messages_are_not_special_cased_as_continuation() {
        let suggestion = suggest_followup(
            "继续",
            &base_state(),
            &base_result(vec!["str_replace"], "Patched the file."),
        )
        .expect("edit follow-up suggestion");
        assert_eq!(suggestion.text, "跑一下测试");
        assert_eq!(suggestion.kind, FollowupSuggestionKind::Validate);
    }
}
