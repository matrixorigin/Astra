use super::{ReplState, StreamResult};
pub(crate) use astra_runtime::turn::followup_suggestion::FollowupSuggestion;
#[cfg(test)]
use astra_runtime::turn::followup_suggestion::FollowupSuggestionKind;

pub(crate) fn suggest_followup(
    line: &str,
    state: &ReplState,
    result: &StreamResult,
) -> Option<FollowupSuggestion> {
    let trimmed = line.trim();
    if trimmed.is_empty()
        || trimmed.starts_with('/')
        || super::repl_turn::is_short_continuation_prompt(trimmed)
        || state.plan_mode.is_some()
        || state.executing_plan.is_some()
        || state.plan_handle.is_some()
        || state.pending_approval.is_some()
        || state.last_turn_interrupted
    {
        return None;
    }

    astra_runtime::turn::followup_suggestion::suggest_followup(
        trimmed,
        &result.full_text,
        &result.tools_used,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_state() -> ReplState {
        ReplState::default()
    }

    fn base_result(tools_used: Vec<&str>, full_text: &str) -> StreamResult {
        StreamResult {
            session_id: None,
            run_id: None,
            full_text: full_text.to_string(),
            prompt_tokens: 0,
            completion_tokens: 0,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            tool_calls_count: tools_used.len() as u32,
            tools_selected: Vec::new(),
            selected_skills: Vec::new(),
            tools_used: tools_used.into_iter().map(|s| s.to_string()).collect(),
            tool_call_records: Vec::new(),
            budget_used: 0,
            budget_pressure: 0.0,
            stall_events: Vec::new(),
            verdict_events: Vec::new(),
            step_recorder_summary: None,
            tool_health_export: Vec::new(),
            last_heavy_checkpoint: None,
            ttft_ms: None,
            context_ms: None,
            selector_strategy: None,
            selector_ms: None,
            selector_tokens_in: 0,
            selector_tokens_out: 0,
            memoria_ms: None,
            selector_confidence: None,
            routing_domain_hint: None,
            entity_learn_skipped_no_domain: false,
            pending_context_assembly_trace: None,
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
