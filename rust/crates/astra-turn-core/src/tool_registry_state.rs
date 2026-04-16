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
    /// True when the query is a short follow-up referencing previous context
    /// (e.g., "呢" particle, "那" continuation, pronouns referencing prior topic).
    pub is_followup: bool,
    /// True when the query asks about memory/recall/stored data.
    pub is_memory: bool,
    pub turn_count: u32,
    /// Tools used in recent turns — boosts their category score.
    pub recent_tools: Vec<String>,
    /// Disambiguation result computed from extracted signals.
    /// `None` until `disambiguate()` is called.
    pub disambiguation: Option<crate::routing_metrics::IntentDisambiguation>,
}

impl ConversationState {
    /// Count of active binary signals. Low count = low selector confidence.
    /// Used by adaptive threshold: 0 signals → include all dynamic tools.
    pub fn signal_count(&self) -> usize {
        [
            self.is_fetch,
            self.is_mutate,
            self.is_github,
            self.is_git,
            self.is_analytical,
            self.references_history,
            self.is_memory,
        ]
        .iter()
        .filter(|&&x| x)
        .count()
    }
    /// Extract conversation signals from the latest user message.
    pub fn from_message(msg: &str, turn_count: u32) -> Self {
        Self::from_message_with_context(msg, turn_count, &[])
    }

    /// Maximum query length for signal extraction (chars). Queries longer than
    /// this are truncated before processing to prevent O(n²) slowdowns.
    const MAX_SIGNAL_QUERY_LEN: usize = 2000;

    /// Extract signals from the message, also incorporating recent tool usage context.
    pub fn from_message_with_context(msg: &str, turn_count: u32, recent_tools: &[String]) -> Self {
        // Guard: empty or whitespace-only query → conversational (no signals)
        let trimmed = msg.trim();
        if trimmed.is_empty() {
            return Self {
                is_conversational: true,
                turn_count,
                recent_tools: recent_tools.to_vec(),
                ..Default::default()
            };
        }

        // Guard: pure punctuation/emoji (no alphanumeric or CJK content) → conversational
        let has_content = trimmed
            .chars()
            .any(|c| c.is_alphanumeric() || ('\u{4e00}'..='\u{9fff}').contains(&c));
        if !has_content {
            return Self {
                is_conversational: true,
                turn_count,
                recent_tools: recent_tools.to_vec(),
                ..Default::default()
            };
        }

        // Guard: truncate overlong queries to cap O(n²) signal extraction
        let effective_msg = if trimmed.chars().count() > Self::MAX_SIGNAL_QUERY_LEN {
            trimmed
                .chars()
                .take(Self::MAX_SIGNAL_QUERY_LEN)
                .collect::<String>()
        } else {
            trimmed.to_string()
        };

        let msg_lower = effective_msg.to_lowercase();
        let chars: Vec<char> = msg_lower.chars().collect();

        let mut state = Self {
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
                    // Casual variants
                    "刚刚",
                    "前面",
                    "之前说过",
                    "previously",
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
                    // Casual/extended variants
                    "什么原因",
                    "怎样改进",
                    "optimize",
                    "performance",
                    "root cause",
                    "what went wrong",
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
                    "latest",
                    "status",
                    "check",
                    "什么",
                    "有哪些",
                    "fetch",
                    "如何",
                    "怎么样",
                    "哪些",
                    "多少",
                    "tell me",
                    "show me",
                    // Casual variants
                    "看一下",
                    "给我看",
                    "display",
                    "retrieve",
                    "where is",
                ],
            ),
            is_mutate: contains_any(
                &msg_lower,
                &chars,
                &[
                    "创建",
                    "修改",
                    "删除",
                    "写入",
                    "create",
                    "update",
                    "delete",
                    "write",
                    "add",
                    "remove",
                    "fix",
                    "修复",
                    "新建",
                    // Casual variants
                    "改一下",
                    "添加",
                    "改成",
                    "新增",
                    "移除",
                    "加上",
                    "change",
                    "set",
                    "patch",
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
                    "star",
                ],
            ),
            is_followup: is_followup_msg(&msg_lower, &chars, turn_count),
            is_memory: contains_any(
                &msg_lower,
                &chars,
                &[
                    "记忆",
                    "memory",
                    "memories",
                    "记住",
                    "记得",
                    "recall",
                    "记录",
                    "存储",
                    "存了",
                    "存过",
                    // Aligned with system prompt memory triggers
                    "关注",
                    "跟踪",
                    "留意",
                    "感兴趣",
                    "偏好",
                    "follow",
                    "watch",
                    "track",
                    "interested",
                    "prefer",
                    "remember",
                ],
            ),
            turn_count,
            recent_tools: recent_tools.to_vec(),
            disambiguation: None,
        };

        // Follow-up with "呢" implicitly inherits fetch intent (asking "what about X?")
        if state.is_followup && !state.is_fetch {
            state.is_fetch = true;
        }

        // Run intent disambiguation on the extracted signals
        let disambig = crate::routing_metrics::disambiguate_intents(
            state.is_fetch,
            state.is_mutate,
            state.is_analytical,
            state.is_github,
            state.is_git,
            state.references_history,
        );
        state.disambiguation = Some(disambig);
        state
    }
}

/// Detect follow-up patterns in short queries.
///
/// Follow-ups are short messages that reference the previous conversation context
/// rather than stating a complete question. Examples:
/// - "呢" particle: "pr呢？" = "what about PRs?" (implies continuation)
/// - "那" continuation: "那star呢？" = "then what about stars?"
/// - "也" comparison: "issue也看看" = "look at issues too"
/// - Short queries on non-first turns with entity names but no verb
///
/// UNIVERSAL rule: length-gated (≤15 chars) + continuation particle/pattern.
fn is_followup_msg(lower: &str, chars: &[char], turn_count: u32) -> bool {
    // First turn cannot be a follow-up
    if turn_count <= 1 {
        return false;
    }

    let len = chars.len();

    // Chinese continuation particles — very strong follow-up signal
    let cn_particles = ["呢", "那", "也", "还有", "另外", "同样", "一样", "最新"];
    if len <= 15 && cn_particles.iter().any(|p| lower.contains(p)) {
        return true;
    }

    // English follow-up patterns
    let en_patterns = [
        "what about",
        "how about",
        "and the",
        "same for",
        "also",
        "too?",
        "as well",
    ];
    if len <= 30 && en_patterns.iter().any(|p| lower.contains(p)) {
        return true;
    }

    // Very short queries (≤8 chars) on non-first turns with a question mark
    // e.g., "pr呢？", "star?", "issues?"
    if len <= 8 && (lower.contains('?') || lower.contains('？')) {
        return true;
    }

    false
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
/// Uses lightweight stemming: strips common English suffixes (-s, -es, -ing, -ed, -tion)
/// so keywords like "commit" match "commits", "committing", "committed".
/// This is a UNIVERSAL rule — no per-keyword plural/tense additions needed.
pub fn word_boundary_match(haystack: &str, _chars: &[char], needle: &str) -> bool {
    let needle_lower = needle.to_lowercase();

    // CJK / non-ASCII needle: use simple substring matching
    // CJK characters don't have word boundaries or stemming rules
    if !needle_lower.is_ascii() {
        return haystack.contains(&needle_lower);
    }

    // Tokenize haystack into words and check if any word stem-matches the needle
    for word in haystack.split(|c: char| !c.is_ascii_alphanumeric() && c != '_') {
        if word.is_empty() {
            continue;
        }
        // Exact match
        if word == needle_lower {
            return true;
        }
        // Multi-word needle: fall back to substring boundary matching
        if needle_lower.contains(' ') {
            if substring_boundary_match(haystack, &needle_lower) {
                return true;
            }
            continue;
        }
        // Stem match: does the word reduce to the needle after stripping suffixes?
        if stem_matches(word, &needle_lower) {
            return true;
        }
    }
    // Multi-word needle fallback (if the loop didn't catch it)
    if needle_lower.contains(' ') {
        return substring_boundary_match(haystack, &needle_lower);
    }
    false
}

/// Lightweight English stemming: does `word` reduce to `stem` after stripping
/// common suffixes? Handles:
///   - Plurals: -s, -es (commit→commits, issue→issues)
///   - Past tense: -ed (fix→fixed)
///   - Gerund: -ing (debug→debugging)
///   - Consonant doubling: commit→committed/committing
///   - Drop-e: merge→merged/merging, analyze→analyzed
///   - Derivational: -tion, -ation, -ment, -ly, -er, -ors
///
/// NOT a full Porter stemmer — intentionally minimal to avoid false positives.
fn stem_matches(word: &str, stem: &str) -> bool {
    if word == stem {
        return true;
    }
    if word.len() <= stem.len() {
        return false;
    }

    // Path 1: word starts with full stem (direct suffixing)
    if let Some(suffix) = word.strip_prefix(stem) {
        if matches!(
            suffix,
            "s" | "es" | "ed" | "ing" | "tion" | "ation" | "ment" | "ly" | "er" | "ors"
        ) {
            return true;
        }
        // Consonant doubling: "commit" → "committed" (suffix="ted"), "committing" (suffix="ting")
        if let Some(last_char) = stem.chars().last()
            && last_char.is_ascii_alphabetic()
        {
            let doubled = format!("{last_char}");
            if let Some(rest) = suffix.strip_prefix(doubled.as_str())
                && matches!(rest, "ed" | "ing" | "er" | "s" | "es")
            {
                return true;
            }
        }
        // Drop-e: stem="merge", suffix="d"/"r"/"s" (merged/merger/merges)
        if stem.ends_with('e') && matches!(suffix, "d" | "r" | "s") {
            return true;
        }
    }

    // Path 2: Drop-e pattern — stem ends in 'e', word uses stem-without-e
    // "merge" → "merging": stem_no_e="merg", word starts with "merg" + "ing"
    // "analyze" → "analyzing": stem_no_e="analyz", word starts with "analyz" + "ing"
    if stem.ends_with('e') && stem.len() > 1 {
        let stem_no_e = &stem[..stem.len() - 1];
        if let Some(suffix) = word.strip_prefix(stem_no_e)
            && matches!(suffix, "ing" | "ed" | "er" | "ation" | "able")
        {
            return true;
        }
    }

    false
}

/// Substring match with word-boundary check for multi-word needles.
/// End boundary allows common English suffixes (stemming-aware).
fn substring_boundary_match(haystack: &str, needle: &str) -> bool {
    let mut start = 0;
    while let Some(pos) = haystack[start..].find(needle) {
        let abs_pos = start + pos;
        let end_pos = abs_pos + needle.len();
        let at_start = abs_pos == 0
            || !haystack
                .as_bytes()
                .get(abs_pos - 1)
                .map(|b| b.is_ascii_alphanumeric() || *b == b'_')
                .unwrap_or(false);
        // End boundary: exact boundary OR followed by a common suffix then boundary
        let at_end = if end_pos >= haystack.len() {
            true
        } else {
            let rest = &haystack[end_pos..];
            let next_non_alnum = rest
                .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                .unwrap_or(rest.len());
            let trailing = &rest[..next_non_alnum];
            // Exact boundary (no trailing chars) or trailing is a common suffix
            trailing.is_empty()
                || matches!(
                    trailing,
                    "s" | "es" | "ed" | "ing" | "tion" | "ation" | "ment" | "ly" | "er"
                )
        };
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
    let conversational_cn = ["你好", "谢谢", "再见", "好的", "是的", "不是", "嗯"];
    let conversational_en = [
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
    ];
    // CJK: substring match (safe — CJK characters don't overlap accidentally)
    if conversational_cn.iter().any(|p| lower.contains(p)) {
        return true;
    }
    // ASCII: word-boundary-aware match to avoid false positives
    // (e.g., "this" matching "hi", "tokenbudget" matching "ok")
    conversational_en
        .iter()
        .any(|p| word_boundary_match(lower, chars, p))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ──────────────────────────────────────────────────────────
    // word_boundary_match
    // ──────────────────────────────────────────────────────────

    #[test]
    fn word_boundary_exact_match() {
        let h = "check the git log";
        let chars: Vec<char> = h.chars().collect();
        assert!(word_boundary_match(h, &chars, "git"));
    }

    #[test]
    fn word_boundary_no_false_positive_substring() {
        // "digit" contains "git" but shouldn't match at word boundary
        let h = "digit recognition";
        let chars: Vec<char> = h.chars().collect();
        assert!(!word_boundary_match(h, &chars, "git"));
    }

    #[test]
    fn word_boundary_stem_plural() {
        let h = "list all commits";
        let chars: Vec<char> = h.chars().collect();
        assert!(word_boundary_match(h, &chars, "commit"));
    }

    #[test]
    fn word_boundary_stem_past_tense() {
        let h = "i already fixed it";
        let chars: Vec<char> = h.chars().collect();
        assert!(word_boundary_match(h, &chars, "fix"));
    }

    #[test]
    fn word_boundary_stem_gerund() {
        let h = "currently debugging the issue";
        let chars: Vec<char> = h.chars().collect();
        assert!(word_boundary_match(h, &chars, "debug"));
    }

    #[test]
    fn word_boundary_cjk_substring() {
        let h = "我需要分析这个";
        let chars: Vec<char> = h.chars().collect();
        assert!(word_boundary_match(h, &chars, "分析"));
    }

    #[test]
    fn word_boundary_multi_word_needle() {
        let h = "open a pull request for this";
        let chars: Vec<char> = h.chars().collect();
        assert!(word_boundary_match(h, &chars, "pull request"));
    }

    #[test]
    fn word_boundary_no_match() {
        let h = "hello world";
        let chars: Vec<char> = h.chars().collect();
        assert!(!word_boundary_match(h, &chars, "git"));
    }

    // ──────────────────────────────────────────────────────────
    // stem_matches
    // ──────────────────────────────────────────────────────────

    #[test]
    fn stem_exact() {
        assert!(stem_matches("commit", "commit"));
    }

    #[test]
    fn stem_plural_s() {
        assert!(stem_matches("commits", "commit"));
    }

    #[test]
    fn stem_plural_es() {
        assert!(stem_matches("issues", "issue"));
    }

    #[test]
    fn stem_past_ed() {
        assert!(stem_matches("fixed", "fix"));
    }

    #[test]
    fn stem_gerund_ing() {
        assert!(stem_matches("debugging", "debug"));
    }

    #[test]
    fn stem_consonant_doubling() {
        assert!(stem_matches("committed", "commit"));
        assert!(stem_matches("committing", "commit"));
    }

    #[test]
    fn stem_drop_e_gerund() {
        assert!(stem_matches("merging", "merge"));
        assert!(stem_matches("analyzing", "analyze"));
    }

    #[test]
    fn stem_drop_e_past() {
        assert!(stem_matches("merged", "merge"));
    }

    #[test]
    fn stem_no_match() {
        assert!(!stem_matches("hello", "world"));
    }

    #[test]
    fn stem_shorter_word_no_match() {
        assert!(!stem_matches("co", "commit"));
    }

    // ──────────────────────────────────────────────────────────
    // substring_boundary_match
    // ──────────────────────────────────────────────────────────

    #[test]
    fn substring_boundary_exact() {
        assert!(substring_boundary_match(
            "open pull request now",
            "pull request"
        ));
    }

    #[test]
    fn substring_boundary_with_suffix() {
        assert!(substring_boundary_match(
            "pull requests are pending",
            "pull request"
        ));
    }

    #[test]
    fn substring_boundary_mid_word_no_match() {
        assert!(!substring_boundary_match("xpull request", "pull request"));
    }

    // ──────────────────────────────────────────────────────────
    // is_followup_msg
    // ──────────────────────────────────────────────────────────

    #[test]
    fn followup_first_turn_never() {
        let s = "pr呢？";
        let chars: Vec<char> = s.chars().collect();
        assert!(!is_followup_msg(s, &chars, 1));
    }

    #[test]
    fn followup_chinese_particle() {
        let s = "pr呢？";
        let chars: Vec<char> = s.chars().collect();
        assert!(is_followup_msg(s, &chars, 2));
    }

    #[test]
    fn followup_english_pattern() {
        let s = "what about tests?";
        let chars: Vec<char> = s.chars().collect();
        assert!(is_followup_msg(s, &chars, 3));
    }

    #[test]
    fn followup_short_question_mark() {
        let s = "star?";
        let chars: Vec<char> = s.chars().collect();
        assert!(is_followup_msg(s, &chars, 2));
    }

    #[test]
    fn followup_long_message_not_followup() {
        let s =
            "this is a really long message that has nothing to do with follow-up patterns at all";
        let chars: Vec<char> = s.chars().collect();
        assert!(!is_followup_msg(s, &chars, 5));
    }

    // ──────────────────────────────────────────────────────────
    // is_conversational_msg
    // ──────────────────────────────────────────────────────────

    #[test]
    fn conversational_hello() {
        let s = "hello";
        let chars: Vec<char> = s.chars().collect();
        assert!(is_conversational_msg(s, &chars));
    }

    #[test]
    fn conversational_chinese() {
        let s = "谢谢";
        let chars: Vec<char> = s.chars().collect();
        assert!(is_conversational_msg(s, &chars));
    }

    #[test]
    fn conversational_long_not_conversational() {
        let s = "please explain the architecture of this system in detail";
        let chars: Vec<char> = s.chars().collect();
        assert!(!is_conversational_msg(s, &chars));
    }

    #[test]
    fn conversational_hi_no_false_positive_in_long() {
        // "this" contains "hi" but is too long (>20 chars scenario)
        let s = "this is a technical discussion about something";
        let chars: Vec<char> = s.chars().collect();
        assert!(!is_conversational_msg(s, &chars));
    }

    // ──────────────────────────────────────────────────────────
    // ConversationState::signal_count
    // ──────────────────────────────────────────────────────────

    #[test]
    fn signal_count_default_is_zero() {
        let s = ConversationState::default();
        assert_eq!(s.signal_count(), 0);
    }

    #[test]
    fn signal_count_counts_true_flags() {
        let s = ConversationState {
            is_fetch: true,
            is_git: true,
            is_memory: true,
            ..Default::default()
        };
        assert_eq!(s.signal_count(), 3);
    }

    // ──────────────────────────────────────────────────────────
    // ConversationState::from_message
    // ──────────────────────────────────────────────────────────

    #[test]
    fn from_message_empty() {
        let s = ConversationState::from_message("", 1);
        assert!(s.is_conversational);
    }

    #[test]
    fn from_message_pure_punctuation() {
        let s = ConversationState::from_message("!!??...", 1);
        assert!(s.is_conversational);
    }

    #[test]
    fn from_message_git_signal() {
        let s = ConversationState::from_message("show me the git log", 1);
        assert!(s.is_git);
    }

    #[test]
    fn from_message_github_signal() {
        let s = ConversationState::from_message("open a pull request", 1);
        assert!(s.is_github);
    }

    #[test]
    fn from_message_memory_signal() {
        let s = ConversationState::from_message("记住这个偏好", 1);
        assert!(s.is_memory);
    }

    #[test]
    fn from_message_mutate_signal() {
        let s = ConversationState::from_message("create a new file", 1);
        assert!(s.is_mutate);
    }

    #[test]
    fn from_message_followup_sets_fetch() {
        let s = ConversationState::from_message("pr呢？", 2);
        assert!(s.is_followup);
        assert!(s.is_fetch); // follow-up inherits fetch
    }

    #[test]
    fn from_message_long_query_truncated() {
        let long = "analyze ".repeat(500); // > 2000 chars
        let s = ConversationState::from_message(&long, 1);
        assert!(s.is_analytical); // still detects signal in truncated prefix
    }
}
