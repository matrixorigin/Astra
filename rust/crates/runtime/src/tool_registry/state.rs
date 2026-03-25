/// Lightweight signals extracted from the user message.
/// Pure regex, no LLM cost. Used for pre-filter reordering.
///
/// **ARCHITECTURAL NOTE**: This is an implementation detail of [`TfIdfSelector`](crate::tool_selector::TfIdfSelector).
/// It is a **leaky abstraction** — each edge case requires a new field, effectively
/// simulating a mini language model with struct fields. **Do NOT add new fields.**
/// New edge cases should be handled by improving the LLM tool selector instead.
/// This struct is preserved only as a fast fallback for when LLM selection is unavailable.
#[derive(Debug, Clone, Default)]
pub struct ConversationState {
    pub references_history: bool,
    pub is_analytical: bool,
    pub is_fetch: bool,
    pub is_mutate: bool,
    pub is_conversational: bool,
    pub is_git: bool,
    pub is_github: bool,
    #[allow(dead_code)]
    pub turn_count: u32,
    /// Tools used in recent turns — boosts their category score.
    pub recent_tools: Vec<String>,
}

impl ConversationState {
    /// Extract conversation signals from the latest user message.
    #[allow(dead_code)]
    pub fn from_message(msg: &str, turn_count: u32) -> Self {
        Self::from_message_with_context(msg, turn_count, &[])
    }

    /// Extract signals from the message, also incorporating recent tool usage context.
    pub fn from_message_with_context(msg: &str, turn_count: u32, recent_tools: &[String]) -> Self {
        let msg_lower = msg.to_lowercase();
        let chars: Vec<char> = msg.chars().collect();

        Self {
            references_history: contains_any(
                &msg_lower,
                &chars,
                &[
                    "前一个",
                    "上一轮",
                    "刚才",
                    "之前",
                    "earlier",
                    "previous",
                    "last time",
                    "上次",
                    "历史",
                    "before",
                ],
            ),
            is_analytical: contains_any(
                &msg_lower,
                &chars,
                &[
                    "分析",
                    "评估",
                    "为什么",
                    "怎么回事",
                    "analyze",
                    "why",
                    "investigate",
                    "explain",
                    "debug",
                    "解释",
                    "诊断",
                    "原因",
                ],
            ),
            is_fetch: contains_any(
                &msg_lower,
                &chars,
                &[
                    "查看",
                    "列出",
                    "最新",
                    "情况",
                    "list",
                    "show",
                    "get",
                    "latest",
                    "status",
                    "check",
                    "什么",
                    "有哪些",
                    "fetch",
                    "呢", // follow-up question particle: "X呢？" = "what about X?"
                    "如何",
                    "怎么样",
                    "哪些",
                    "多少",
                    "tell me",
                    "show me",
                ],
            ),
            is_mutate: contains_any(
                &msg_lower,
                &chars,
                &[
                    "创建", "修改", "删除", "写入", "create", "update", "delete", "write", "add",
                    "remove", "fix", "修复", "新建",
                ],
            ),
            is_conversational: is_conversational_msg(&msg_lower, &chars),
            is_git: contains_any(
                &msg_lower,
                &chars,
                &[
                    "git", "diff", "commit", "branch", "merge", "rebase", "stash", "提交", "分支",
                    "合并",
                ],
            ),
            is_github: contains_any(
                &msg_lower,
                &chars,
                &[
                    "github",
                    "pr",
                    "pull request",
                    "issue",
                    "ci",
                    "actions",
                    "仓库",
                    "拉取请求",
                    "repo",
                    "repository",
                ],
            ),
            turn_count,
            recent_tools: recent_tools.to_vec(),
        }
    }
}

fn contains_any(lower: &str, chars: &[char], patterns: &[&str]) -> bool {
    patterns.iter().any(|p| {
        if p.chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c)) {
            // CJK: direct substring match
            lower.contains(p)
        } else {
            // ASCII: word-boundary-aware
            word_boundary_match(lower, chars, p)
        }
    })
}

/// Check if `needle` appears in `haystack` at ASCII word boundaries.
pub(crate) fn word_boundary_match(haystack: &str, _chars: &[char], needle: &str) -> bool {
    let needle_lower = needle.to_lowercase();
    let mut start = 0;
    while let Some(pos) = haystack[start..].find(&needle_lower) {
        let abs_pos = start + pos;
        let end_pos = abs_pos + needle_lower.len();
        let at_start = abs_pos == 0
            || !haystack
                .as_bytes()
                .get(abs_pos - 1)
                .map(|b| b.is_ascii_alphanumeric() || *b == b'_')
                .unwrap_or(false);
        let at_end = end_pos >= haystack.len()
            || !haystack
                .as_bytes()
                .get(end_pos)
                .map(|b| b.is_ascii_alphanumeric() || *b == b'_')
                .unwrap_or(false);
        if at_start && at_end {
            return true;
        }
        start = abs_pos + 1;
    }
    false
}

fn is_conversational_msg(lower: &str, chars: &[char]) -> bool {
    if chars.len() > 20 {
        return false;
    }
    let conversational = [
        "hello",
        "hi",
        "hey",
        "thanks",
        "thank you",
        "bye",
        "goodbye",
        "yes",
        "no",
        "ok",
        "okay",
        "sure",
        "yep",
        "nope",
        "你好",
        "谢谢",
        "再见",
        "好的",
        "是的",
        "不是",
        "嗯",
    ];
    conversational.iter().any(|p| lower.contains(p))
}
