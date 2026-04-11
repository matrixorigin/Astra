use super::{ReplState, StreamResult};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FollowupSuggestionKind {
    Validate,
    Commit,
    Push,
    Continue,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FollowupSuggestion {
    pub(crate) text: String,
    pub(crate) kind: FollowupSuggestionKind,
}

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
        || result.full_text.trim().is_empty()
        || assistant_looks_incomplete(&result.full_text)
    {
        return None;
    }

    let lexicon = suggestion_lexicon(trimmed, &result.full_text);
    let edited = result.tools_used.iter().any(|tool| is_edit_tool(tool));
    let validated = result
        .tools_used
        .iter()
        .any(|tool| is_validation_tool(tool));
    let committed = result.tools_used.iter().any(|tool| tool == "git_commit");

    if let Some(question_reply) = suggest_reply_to_assistant_question(
        &result.full_text,
        edited,
        validated,
        committed,
        &lexicon,
    ) {
        return Some(question_reply);
    }

    if assistant_requests_reply(&result.full_text) {
        return None;
    }

    if committed {
        return Some(FollowupSuggestion {
            text: lexicon.push.to_string(),
            kind: FollowupSuggestionKind::Push,
        });
    }

    if edited && validated {
        return Some(FollowupSuggestion {
            text: lexicon.commit.to_string(),
            kind: FollowupSuggestionKind::Commit,
        });
    }

    if edited {
        return Some(FollowupSuggestion {
            text: lexicon.validate.to_string(),
            kind: FollowupSuggestionKind::Validate,
        });
    }

    None
}

struct SuggestionLexicon {
    validate: &'static str,
    commit: &'static str,
    push: &'static str,
    continue_prompt: &'static str,
}

fn suggestion_lexicon(line: &str, assistant_text: &str) -> SuggestionLexicon {
    if prefers_chinese(line) || prefers_chinese(assistant_text) {
        SuggestionLexicon {
            validate: "跑一下测试",
            commit: "提交一下",
            push: "推上去",
            continue_prompt: "继续",
        }
    } else {
        SuggestionLexicon {
            validate: "run the tests",
            commit: "commit this",
            push: "push it",
            continue_prompt: "go ahead",
        }
    }
}

fn prefers_chinese(text: &str) -> bool {
    text.chars()
        .any(|ch| ('\u{4E00}'..='\u{9FFF}').contains(&ch))
}

fn is_edit_tool(tool: &str) -> bool {
    matches!(
        tool,
        "write_file"
            | "str_replace"
            | "multi_edit"
            | "create_file"
            | "delete_file"
            | "move_file"
            | "git_commit"
    )
}

fn is_validation_tool(tool: &str) -> bool {
    matches!(tool, "run_build_test")
}

fn assistant_requests_reply(full_text: &str) -> bool {
    let last_line = full_text
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or(full_text)
        .trim();
    if last_line.ends_with('?') || last_line.ends_with('？') {
        return true;
    }

    let lower = full_text.to_ascii_lowercase();
    lower.contains("would you like")
        || lower.contains("do you want")
        || lower.contains("should i")
        || lower.contains("want me to")
        || lower.contains("which option")
        || lower.contains("which one")
        || full_text.contains("要我")
        || full_text.contains("你想")
        || full_text.contains("是否需要")
        || full_text.contains("要不要")
}

fn suggest_reply_to_assistant_question(
    full_text: &str,
    edited: bool,
    validated: bool,
    committed: bool,
    lexicon: &SuggestionLexicon,
) -> Option<FollowupSuggestion> {
    if !assistant_requests_reply(full_text) {
        return None;
    }

    let lower = full_text.to_ascii_lowercase();
    if committed && mentions_push_question(&lower, full_text) {
        return Some(FollowupSuggestion {
            text: lexicon.push.to_string(),
            kind: FollowupSuggestionKind::Push,
        });
    }

    if edited && validated && mentions_commit_question(&lower, full_text) {
        return Some(FollowupSuggestion {
            text: lexicon.commit.to_string(),
            kind: FollowupSuggestionKind::Commit,
        });
    }

    if edited && mentions_test_question(&lower, full_text) {
        return Some(FollowupSuggestion {
            text: lexicon.validate.to_string(),
            kind: FollowupSuggestionKind::Validate,
        });
    }

    if mentions_continue_question(&lower, full_text) {
        return Some(FollowupSuggestion {
            text: lexicon.continue_prompt.to_string(),
            kind: FollowupSuggestionKind::Continue,
        });
    }

    None
}

fn mentions_continue_question(lower: &str, full_text: &str) -> bool {
    lower.contains("continue")
        || lower.contains("keep going")
        || lower.contains("keep working")
        || lower.contains("go ahead")
        || full_text.contains("继续")
        || full_text.contains("接着")
        || full_text.contains("往下")
}

fn mentions_test_question(lower: &str, full_text: &str) -> bool {
    lower.contains("run the tests")
        || lower.contains("run tests")
        || lower.contains("run the test")
        || lower.contains("test this")
        || lower.contains("verify it")
        || full_text.contains("测试")
        || full_text.contains("验证")
}

fn mentions_commit_question(lower: &str, full_text: &str) -> bool {
    lower.contains("commit") || full_text.contains("提交")
}

fn mentions_push_question(lower: &str, full_text: &str) -> bool {
    lower.contains("push") || full_text.contains("推上去") || full_text.contains("推到远端")
}

fn assistant_looks_incomplete(full_text: &str) -> bool {
    let lower = full_text.to_ascii_lowercase();
    lower.contains("error:")
        || lower.contains("i couldn't")
        || lower.contains("i could not")
        || lower.contains("i can’t")
        || lower.contains("unable to")
        || lower.contains("failed to")
        || full_text.contains("失败")
        || full_text.contains("出错")
        || full_text.contains("无法")
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
