#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FollowupSuggestionKind {
    Validate,
    Commit,
    Push,
    Continue,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FollowupSuggestion {
    pub text: String,
    pub kind: FollowupSuggestionKind,
}

pub fn suggest_followup(
    user_message: &str,
    assistant_text: &str,
    tool_names: &[String],
) -> Option<FollowupSuggestion> {
    let trimmed = user_message.trim();
    if trimmed.is_empty()
        || trimmed.starts_with('/')
        || assistant_text.trim().is_empty()
        || assistant_looks_incomplete(assistant_text)
    {
        return None;
    }

    let lexicon = suggestion_lexicon(trimmed, assistant_text);
    let edited = tool_names.iter().any(|tool| is_edit_tool(tool));
    let validated = tool_names.iter().any(|tool| is_validation_tool(tool));
    let committed = tool_names.iter().any(|tool| tool == "git_commit");

    if let Some(question_reply) =
        suggest_reply_to_assistant_question(assistant_text, edited, validated, committed, &lexicon)
    {
        return Some(question_reply);
    }

    if assistant_requests_reply(assistant_text) {
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

    #[test]
    fn suggests_validation_after_edit_turn() {
        let suggestion = suggest_followup(
            "fix the bug",
            "Fixed the bug.",
            &["str_replace".to_string()],
        )
        .expect("suggestion");
        assert_eq!(suggestion.text, "run the tests");
        assert_eq!(suggestion.kind, FollowupSuggestionKind::Validate);
    }

    #[test]
    fn suggests_chinese_validation_after_edit_turn() {
        let suggestion = suggest_followup(
            "修一下这个 bug",
            "已经修好了。",
            &["str_replace".to_string()],
        )
        .expect("suggestion");
        assert_eq!(suggestion.text, "跑一下测试");
    }

    #[test]
    fn suppresses_meta_question_without_obvious_next_action() {
        assert_eq!(
            suggest_followup(
                "fix the bug",
                "I have two valid options. Which one do you want me to try?",
                &["str_replace".to_string()],
            ),
            None
        );
    }
}
