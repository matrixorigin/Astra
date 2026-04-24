//! Factual-query detection, session error classification, and memory repo extraction.
//! Shared by CLI `chat_stream` / `repl_turn` and available for in-process bridge parity tests.

use std::collections::HashSet;
use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

use crate::chat_history_openai::openai_user_content_message;
use crate::interaction_types::{ASK_USER_TOOL_NAME, TurnInteractionPolicy};

const DEFAULT_STALL_WINDOW: usize = 3;
const DEFAULT_EXPLORATION_ROUND_BUDGET: usize = 5;
const EXPLORATORY_STALL_WINDOW: usize = 4;
const EXPLORATORY_ROUND_BUDGET: usize = 8;

const STANDARD_ANALYSIS_TURN_BUDGET: AgenticTurnBudget = AgenticTurnBudget::new(8, 12, 2, 2);
const COMPLEX_ANALYSIS_TURN_BUDGET: AgenticTurnBudget = AgenticTurnBudget::new(10, 18, 4, 2);
const STANDARD_IMPLEMENTATION_TURN_BUDGET: AgenticTurnBudget = AgenticTurnBudget::new(12, 20, 4, 2);
const COMPLEX_IMPLEMENTATION_TURN_BUDGET: AgenticTurnBudget = AgenticTurnBudget::new(16, 32, 4, 4);
const STANDARD_EXPLORATORY_TURN_BUDGET: AgenticTurnBudget = AgenticTurnBudget::new(12, 24, 4, 3);
const COMPLEX_EXPLORATORY_TURN_BUDGET: AgenticTurnBudget = AgenticTurnBudget::new(16, 36, 5, 4);
const STANDARD_MUTATING_EXPLORATORY_TURN_BUDGET: AgenticTurnBudget =
    AgenticTurnBudget::new(14, 28, 4, 3);
const COMPLEX_MUTATING_EXPLORATORY_TURN_BUDGET: AgenticTurnBudget =
    AgenticTurnBudget::new(18, 40, 6, 4);

const MUTATING_TERMS: &[&str] = &[
    "fix",
    "修改代码",
    "修改文件",
    "implement",
    "write",
    "edit",
    "change code",
    "change the code",
    "update code",
    "create",
    "add ",
    "remove",
    "delete",
    "patch",
    "refactor",
    "apply",
    "rename",
    "重构",
    "实现",
    "新增",
    "删除",
    "更新代码",
    "更新文件",
    "修复",
    "修正",
];

const ANALYSIS_TERMS: &[&str] = &[
    "review",
    "code review",
    "commit review",
    "审查",
    "评审",
    "审阅",
    "看一眼",
    "看一下",
    "看改动",
    "看修改",
    "看变更",
    "what changed",
    "changed",
    "what's in the commit",
    "summarize",
    "summary",
    "explain",
    "inspect",
    "analyze",
    "search",
    "status",
    "diff",
    "show",
    "read",
    "list",
    "latest commit",
    "看看",
    "解释",
    "分析",
    "总结",
    "搜索",
    "列出",
];

const EXPLORATION_TERMS: &[&str] = &[
    "explore",
    "exploration",
    "investigate",
    "deep dive",
    "understand the codebase",
    "understand this system",
    "trace",
    "root cause",
    "walk through",
    "map out",
    "survey",
    "dig into",
    "探索",
    "排查",
    "调研",
    "梳理",
    "追踪",
    "根因",
    "深挖",
    "看源码",
];

const COMPLEXITY_TERMS: &[&str] = &[
    "complex",
    "systematic",
    "end-to-end",
    "multi-step",
    "architecture",
    "subsystem",
    "large refactor",
    "cross-cutting",
    "全链路",
    "系统性",
    "复杂",
    "大型",
    "架构",
    "重构",
    "整体",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskComplexity {
    Standard,
    Complex,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgenticTurnBudget {
    pub initial_turns: usize,
    pub hard_turn_limit: usize,
    pub extension_turns: usize,
    pub max_extensions: u32,
}

impl AgenticTurnBudget {
    pub const fn new(
        initial_turns: usize,
        hard_turn_limit: usize,
        extension_turns: usize,
        max_extensions: u32,
    ) -> Self {
        Self {
            initial_turns,
            hard_turn_limit,
            extension_turns,
            max_extensions,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AgenticTurnBudgetOverride {
    pub initial_turns: Option<usize>,
    pub hard_turn_limit: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TaskExecutionProfile {
    pub mutates_workspace: bool,
    pub verification_required: bool,
    pub allow_factual_retry: bool,
    pub exploration_round_budget: usize,
    pub stall_window: usize,
    pub complexity: TaskComplexity,
    pub exploratory_task: bool,
    pub agentic_turn_budget: AgenticTurnBudget,
}

impl Default for TaskExecutionProfile {
    fn default() -> Self {
        Self {
            mutates_workspace: false,
            verification_required: false,
            allow_factual_retry: true,
            exploration_round_budget: DEFAULT_EXPLORATION_ROUND_BUDGET,
            stall_window: DEFAULT_STALL_WINDOW,
            complexity: TaskComplexity::Standard,
            exploratory_task: false,
            agentic_turn_budget: STANDARD_ANALYSIS_TURN_BUDGET,
        }
    }
}

fn contains_any_keyword(q: &str, keywords: &[&str]) -> bool {
    keywords.iter().any(|kw| q.contains(kw))
}

#[must_use]
pub fn infer_task_execution_profile(input: &str) -> TaskExecutionProfile {
    let q = input.to_lowercase();
    let has_mutating = contains_any_keyword(&q, MUTATING_TERMS);
    let has_analysis = contains_any_keyword(&q, ANALYSIS_TERMS);
    let exploratory = contains_any_keyword(&q, EXPLORATION_TERMS);
    let complexity = if contains_any_keyword(&q, COMPLEXITY_TERMS) {
        TaskComplexity::Complex
    } else {
        TaskComplexity::Standard
    };
    let exploration_round_budget = if exploratory {
        EXPLORATORY_ROUND_BUDGET
    } else {
        DEFAULT_EXPLORATION_ROUND_BUDGET
    };
    let stall_window = if exploratory || complexity == TaskComplexity::Complex {
        EXPLORATORY_STALL_WINDOW
    } else {
        DEFAULT_STALL_WINDOW
    };
    // When both mutating and analysis terms are present, analysis wins —
    // the user is asking to *review/inspect* something that involves changes,
    // not asking to *make* changes. E.g. "评审当前修改" = review changes.
    if has_mutating && !has_analysis {
        TaskExecutionProfile {
            mutates_workspace: true,
            verification_required: true,
            allow_factual_retry: true,
            exploration_round_budget,
            stall_window,
            complexity,
            exploratory_task: exploratory,
            agentic_turn_budget: default_agentic_turn_budget(true, exploratory, complexity),
        }
    } else if has_analysis {
        TaskExecutionProfile {
            mutates_workspace: false,
            verification_required: false,
            allow_factual_retry: true,
            exploration_round_budget,
            stall_window,
            complexity,
            exploratory_task: exploratory,
            agentic_turn_budget: default_agentic_turn_budget(false, exploratory, complexity),
        }
    } else if has_mutating {
        // Mutating without analysis context
        TaskExecutionProfile {
            mutates_workspace: true,
            verification_required: true,
            allow_factual_retry: true,
            exploration_round_budget,
            stall_window,
            complexity,
            exploratory_task: exploratory,
            agentic_turn_budget: default_agentic_turn_budget(true, exploratory, complexity),
        }
    } else {
        TaskExecutionProfile {
            exploration_round_budget,
            stall_window,
            complexity,
            exploratory_task: exploratory,
            agentic_turn_budget: default_agentic_turn_budget(false, exploratory, complexity),
            ..TaskExecutionProfile::default()
        }
    }
}

fn default_agentic_turn_budget(
    mutates_workspace: bool,
    exploratory: bool,
    complexity: TaskComplexity,
) -> AgenticTurnBudget {
    match (mutates_workspace, exploratory, complexity) {
        (false, false, TaskComplexity::Standard) => STANDARD_ANALYSIS_TURN_BUDGET,
        (false, false, TaskComplexity::Complex) => COMPLEX_ANALYSIS_TURN_BUDGET,
        (true, false, TaskComplexity::Standard) => STANDARD_IMPLEMENTATION_TURN_BUDGET,
        (true, false, TaskComplexity::Complex) => COMPLEX_IMPLEMENTATION_TURN_BUDGET,
        (false, true, TaskComplexity::Standard) => STANDARD_EXPLORATORY_TURN_BUDGET,
        (false, true, TaskComplexity::Complex) => COMPLEX_EXPLORATORY_TURN_BUDGET,
        (true, true, TaskComplexity::Standard) => STANDARD_MUTATING_EXPLORATORY_TURN_BUDGET,
        (true, true, TaskComplexity::Complex) => COMPLEX_MUTATING_EXPLORATORY_TURN_BUDGET,
    }
}

#[must_use]
pub fn resolve_agentic_turn_budget(
    profile: TaskExecutionProfile,
    runtime_ceiling: usize,
    override_budget: Option<AgenticTurnBudgetOverride>,
) -> AgenticTurnBudget {
    let ceiling = runtime_ceiling.max(1);
    let mut budget = profile.agentic_turn_budget;
    let requested_hard = override_budget
        .and_then(|value| value.hard_turn_limit)
        .unwrap_or(budget.hard_turn_limit);
    let hard_turn_limit = requested_hard.max(1).min(ceiling);
    let requested_initial = override_budget
        .and_then(|value| value.initial_turns)
        .unwrap_or(budget.initial_turns);
    let initial_turns = requested_initial.max(1).min(hard_turn_limit);
    let headroom = hard_turn_limit.saturating_sub(initial_turns);
    let extension_turns = if headroom == 0 {
        0
    } else {
        budget.extension_turns.max(1).min(headroom)
    };
    let max_extensions = if extension_turns == 0 {
        0
    } else {
        let max_possible = headroom.div_ceil(extension_turns) as u32;
        budget.max_extensions.min(max_possible).max(1)
    };
    budget.initial_turns = initial_turns;
    budget.hard_turn_limit = hard_turn_limit;
    budget.extension_turns = extension_turns;
    budget.max_extensions = max_extensions;
    budget
}

/// Cloud API returned no such session (case-insensitive substring match).
pub fn is_session_not_found_error(error: &str) -> bool {
    error.to_lowercase().contains("session not found")
}

/// Detect queries that refer to the conversation itself rather than
/// external data.  These should NOT be forced into tool-retry because
/// the answer is already in the LLM's context window.
fn references_conversation_context(input: &str) -> bool {
    let q = input.to_lowercase();
    [
        "上面",
        "上一个",
        "上个问题",
        "之前说的",
        "刚才说的",
        "前面说的",
        "你说的",
        "我说的",
        "我的问题",
        "my question",
        "you said",
        "i said",
        "above",
        "previous message",
        "previous question",
        "previous answer",
    ]
    .iter()
    .any(|kw| q.contains(kw))
}

/// Detect queries that almost certainly need tool calls to answer correctly.
/// Used for the hallucination guard: if LLM answers these with 0 tool calls,
/// the response is likely fabricated.
pub fn looks_like_factual_query(input: &str) -> bool {
    // Queries about the conversation itself never need tools — the LLM
    // already has the full chat history in context.
    if references_conversation_context(input) {
        return false;
    }

    let q = input.to_lowercase();
    // NOTE: bare "问题" removed — it means both "question" and "issue" in
    // Chinese, causing false positives on conversational queries like
    // "我上面一个问题？".  "issue" (English) is specific enough.
    let github_keywords = [
        "pr",
        "pull request",
        "issue",
        "拉取请求",
        "commit",
        "提交",
        "ci ",
        " ci?",
        "ci状态",
        "最新的一个ci",
        "workflow",
        "工作流",
        "pipeline",
        "merge",
        "branch",
        "分支",
        "release",
        "tag",
        "star",
        "stars",
        "多少star",
    ];
    let has_github = github_keywords.iter().any(|kw| q.contains(kw));
    let memory_keywords = ["记忆", "memory", "memories", "存了什么", "记住了什么"];
    let has_memory = memory_keywords.iter().any(|kw| q.contains(kw));
    let git_live_keywords = [
        "git status",
        "git diff",
        "改了什么",
        "有哪些修改",
        "当前有哪些修改",
    ];
    let has_git_live = git_live_keywords.iter().any(|kw| q.contains(kw));
    let code_keywords = [
        "read file",
        "cat ",
        "show me the code",
        "what's in",
        "file content",
    ];
    let has_code = code_keywords.iter().any(|kw| q.contains(kw));
    let web_keywords = ["http", "url", "api ", "endpoint", "fetch", "download"];
    let has_web = web_keywords.iter().any(|kw| q.contains(kw));

    // Workspace-state queries: anything asking about local changes, diffs,
    // file contents, or repo state that the model cannot know without tools.
    // NOTE: bare "看一下" / "看看" removed — they are generic Chinese for
    // "take a look" and trigger on non-workspace queries like "看看你说的对不对".
    // They remain in ANALYSIS_TERMS for task-profile classification.
    let workspace_keywords = [
        "review",
        "diff",
        "changes",
        "changed",
        "local",
        "改动",
        "修改",
        "变更",
        "审查",
        "审阅",
        "评审",
        "什么文件",
        "哪些文件",
        "this repo",
        "this project",
        "codebase",
        "这个项目",
        "这个仓库",
        "代码库",
    ];
    let has_workspace = workspace_keywords.iter().any(|kw| q.contains(kw));

    has_github || has_memory || has_git_live || has_code || has_web || has_workspace
}

/// Detect requests that are likely to mutate the workspace and therefore
/// benefit from verification stop hooks before the agent completes.
///
/// The default should be conservative in the user's favor: read-only tasks
/// (review, explain, search, summarize, inspect) should not be forced through
/// an extra verification round, while implementation/editing tasks should.
pub fn looks_like_mutating_task(input: &str) -> bool {
    let q = input.to_lowercase();
    let has_mutating = contains_any_keyword(&q, MUTATING_TERMS);
    let has_read_only = contains_any_keyword(&q, ANALYSIS_TERMS);
    // Analysis context overrides mutating terms — user is reviewing/inspecting
    // something that involves changes, not requesting changes themselves.
    if has_read_only {
        return false;
    }
    has_mutating
}

fn recent_tools_imply_live_domain(recent_tools: &[String]) -> bool {
    recent_tools.iter().any(|tool| {
        tool.starts_with("github_")
            || tool.starts_with("memory_")
            || matches!(tool.as_str(), "git_status" | "git_diff")
    })
}

pub fn looks_like_live_query_with_context(input: &str, recent_tools: &[String]) -> bool {
    if looks_like_factual_query(input) {
        return true;
    }

    if !recent_tools_imply_live_domain(recent_tools) {
        return false;
    }

    let q = input.trim().to_lowercase();
    let is_short_followup = q.chars().count() <= 12;
    if !is_short_followup {
        return false;
    }

    [
        "最新",
        "latest",
        "那",
        "呢",
        "还有",
        "然后",
        "继续",
        "what about",
        "how about",
    ]
    .iter()
    .any(|kw| q.contains(kw))
}

pub fn should_force_factual_tool_retry(
    profile: TaskExecutionProfile,
    input: &str,
    recent_tools: &[String],
    total_evidence_tool_calls: u32,
    already_retried: bool,
    policy: &TurnInteractionPolicy,
) -> bool {
    profile.allow_factual_retry
        && !already_retried
        && total_evidence_tool_calls == 0
        && policy.has_evidence_tools()
        && looks_like_live_query_with_context(input, recent_tools)
}

pub fn factual_tool_retry_message(original_query: &str, policy: &TurnInteractionPolicy) -> String {
    let tools_hint = if policy.evidence_tool_names.is_empty() {
        "This turn does not expose any evidence-gathering tools. Do not retry with bash or invented tools."
            .to_string()
    } else {
        format!(
            "You have exactly these evidence tools available for this turn: {}.\n\
Only call tools from this list to gather evidence — do not use bash to work around missing tools.",
            policy.evidence_tool_names.join(", ")
        )
    };
    let clarification_hint = if policy.allow_ask_user
        && policy
            .visible_tool_names
            .iter()
            .any(|name| name == ASK_USER_TOOL_NAME)
    {
        "\nIf the request is genuinely ambiguous, you may use ask_user for clarification, but ask_user does not replace evidence collection."
    } else if !policy.can_pause_for_user {
        "\nThis turn cannot pause for user clarification."
    } else {
        ""
    };
    format!(
        "Runtime correction: your previous response answered without using any tools. \
        This query requires live data from the workspace, repository, or external sources \
        that you cannot know from training data alone. Retry from scratch and call tools first.\n\
        \n\
        {tools_hint}\n\
        {clarification_hint}\n\
        \n\
        Discard your previous draft and gather evidence with tools before answering.\n\
        \n\
        Original user query: {original_query}"
    )
}

/// OpenAI `messages` entry for [`factual_tool_retry_message`].
#[must_use]
pub fn openai_factual_tool_retry_user_message(
    original_query: &str,
    policy: &TurnInteractionPolicy,
) -> Value {
    openai_user_content_message(&factual_tool_retry_message(original_query, policy))
}

/// Extract `owner/repo` patterns from memory text.
pub fn extract_repos_from_memory(text: &str) -> Vec<String> {
    static GITHUB_URL_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)github\.com/([a-zA-Z0-9][\w-]{0,38})/([a-zA-Z0-9][\w.-]{0,99})")
            .expect("github url regex")
    });

    static BARE_REPO_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)\b([a-zA-Z0-9][\w-]{0,38})/([a-zA-Z0-9][\w.-]{0,99})\b")
            .expect("repo regex")
    });

    let mut repos = Vec::new();
    let mut seen = HashSet::new();

    let mut add = |owner: &str, repo: &str| {
        let full = format!("{owner}/{repo}");
        let key = full.to_lowercase();
        if seen.insert(key) {
            repos.push(full);
        }
    };

    for cap in GITHUB_URL_RE.captures_iter(text) {
        add(&cap[1], &cap[2]);
    }

    for cap in BARE_REPO_RE.captures_iter(text) {
        let owner = &cap[1];
        let repo = &cap[2];
        if [
            "http", "https", "ftp", "ssh", "git", "usr", "etc", "var", "tmp", "home",
        ]
        .contains(&owner.to_lowercase().as_str())
        {
            continue;
        }
        if owner.contains('.') {
            continue;
        }
        let match_start = cap.get(0).expect("group 0 always exists").start();
        if text[..match_start].ends_with('@') {
            continue;
        }
        add(owner, repo);
    }

    repos
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interaction_types::TurnInteractionMode;

    #[test]
    fn factual_query_detects_github_keywords() {
        assert!(looks_like_factual_query("show me the latest PR"));
        assert!(looks_like_factual_query("list open issues"));
        assert!(looks_like_factual_query("check CI status"));
        assert!(looks_like_factual_query("what's in the commit?"));
        assert!(looks_like_factual_query("workflow status"));
        assert!(looks_like_factual_query("最新的一个ci?"));
        assert!(looks_like_factual_query("多少star了？"));
        assert!(looks_like_factual_query("pr呢？"));
    }

    #[test]
    fn factual_query_detects_file_keywords() {
        assert!(looks_like_factual_query("read file src/main.rs"));
        assert!(looks_like_factual_query("cat the config"));
        assert!(looks_like_factual_query("show me the code in lib.rs"));
    }

    #[test]
    fn factual_query_detects_web_keywords() {
        assert!(looks_like_factual_query("fetch the API endpoint"));
        assert!(looks_like_factual_query("check http://example.com"));
    }

    #[test]
    fn factual_query_detects_memory_and_git_live_queries() {
        assert!(looks_like_factual_query("我有哪些记忆？"));
        assert!(looks_like_factual_query("当前有哪些修改？"));
        assert!(looks_like_factual_query("改了什么，看一眼"));
    }

    #[test]
    fn mutating_task_detection_distinguishes_read_only_flows() {
        assert!(!looks_like_mutating_task("review 最新的commit"));
        assert!(!looks_like_mutating_task(
            "what changed in the latest commit?"
        ));
        assert!(!looks_like_mutating_task("explain this diff"));
        // Analysis context overrides even when mutating terms are present
        assert!(!looks_like_mutating_task(
            "review the latest commit and fix any issues"
        ));
        assert!(!looks_like_mutating_task("评审当前修改"));
        assert!(!looks_like_mutating_task("审查修改"));
        assert!(!looks_like_mutating_task("看一下修改"));
        // Pure mutating without analysis context
        assert!(looks_like_mutating_task("implement the feature"));
        assert!(looks_like_mutating_task("fix the bug"));
        assert!(looks_like_mutating_task("修复这个问题"));
    }

    #[test]
    fn task_execution_profile_scales_budget_for_implementation_and_exploration() {
        let review = infer_task_execution_profile("review 最新的commit");
        assert!(!review.mutates_workspace);
        assert!(!review.verification_required);
        assert!(review.allow_factual_retry);
        assert_eq!(review.stall_window, DEFAULT_STALL_WINDOW);

        let implementation = infer_task_execution_profile("implement the feature");
        assert!(implementation.mutates_workspace);
        assert!(implementation.verification_required);
        assert!(implementation.allow_factual_retry);
        assert!(
            implementation.agentic_turn_budget.initial_turns
                > review.agentic_turn_budget.initial_turns
        );
        assert!(
            implementation.agentic_turn_budget.hard_turn_limit
                > review.agentic_turn_budget.hard_turn_limit
        );

        let exploratory =
            infer_task_execution_profile("explore the codebase and investigate the root cause");
        assert!(exploratory.exploration_round_budget > review.exploration_round_budget);
        assert!(
            exploratory.agentic_turn_budget.hard_turn_limit
                >= implementation.agentic_turn_budget.hard_turn_limit
        );
        assert!(exploratory.exploratory_task);
    }

    #[test]
    fn resolve_agentic_turn_budget_clamps_override_to_runtime_ceiling() {
        let budget = resolve_agentic_turn_budget(
            infer_task_execution_profile("implement the feature"),
            12,
            Some(AgenticTurnBudgetOverride {
                initial_turns: Some(20),
                hard_turn_limit: Some(30),
            }),
        );
        assert_eq!(budget.initial_turns, 12);
        assert_eq!(budget.hard_turn_limit, 12);
        assert_eq!(budget.max_extensions, 0);
    }

    #[test]
    fn chinese_review_queries_get_analysis_profile() {
        // Root cause bug: "评审当前修改" was classified as mutating because
        // single-char "修" in MUTATING_TERMS matched the "修" in "修改".
        // Now fixed: "修" removed from MUTATING_TERMS; "评审" added to ANALYSIS_TERMS.
        let cases = [
            "评审当前修改",
            "审查修改",
            "审阅代码修改",
            "看一下修改",
            "看改动",
            "看看变更",
        ];
        for input in &cases {
            let profile = infer_task_execution_profile(input);
            assert!(
                !profile.verification_required,
                "{input:?} should NOT require verification"
            );
            assert!(
                !profile.mutates_workspace,
                "{input:?} should NOT be classified as mutating"
            );
        }
    }

    #[test]
    fn analysis_wins_over_mutating_when_both_present() {
        // "review and fix" → analysis wins (user is reviewing, not just fixing)
        let profile = infer_task_execution_profile("review the code and fix issues");
        assert!(!profile.verification_required);
        // Pure mutating still works
        let profile = infer_task_execution_profile("fix the compilation error");
        assert!(profile.verification_required);
    }

    #[test]
    fn factual_query_rejects_general_questions() {
        assert!(!looks_like_factual_query("what is Rust?"));
        assert!(!looks_like_factual_query("explain monads"));
        assert!(!looks_like_factual_query("write a function"));
        assert!(!looks_like_factual_query("hello"));
    }

    #[test]
    fn factual_query_rejects_conversation_references() {
        // Queries about the conversation itself should NOT trigger factual retry
        // — the answer is already in the LLM's context window.
        assert!(!looks_like_factual_query("我上面一个问题？"));
        assert!(!looks_like_factual_query("上一个问题是什么"));
        assert!(!looks_like_factual_query("之前说的那个"));
        assert!(!looks_like_factual_query("你说的对吗"));
        assert!(!looks_like_factual_query("what you said above"));
        assert!(!looks_like_factual_query("my previous question"));
    }

    #[test]
    fn factual_query_rejects_ambiguous_chinese_keywords() {
        // "问题" alone should NOT trigger (means "question" not just "issue")
        assert!(!looks_like_factual_query("什么问题？"));
        assert!(!looks_like_factual_query("你有什么问题？"));
        assert!(!looks_like_factual_query("这个有问题吗"));
        // "看一下" / "看看" alone should NOT trigger (too generic)
        assert!(!looks_like_factual_query("看一下这个公式"));
        assert!(!looks_like_factual_query("看看你说的对不对"));
        assert!(!looks_like_factual_query("帮我看看这段话"));
    }

    #[test]
    fn force_retry_only_for_first_zero_tool_factual_answer() {
        let none: Vec<String> = vec![];
        let policy = TurnInteractionPolicy::from_visible_tool_names(
            TurnInteractionMode::Deny,
            vec!["github_ci_status".into()],
        );
        assert!(should_force_factual_tool_retry(
            TaskExecutionProfile::default(),
            "最新的一个ci?",
            &none,
            0,
            false,
            &policy,
        ));
        assert!(!should_force_factual_tool_retry(
            TaskExecutionProfile::default(),
            "最新的一个ci?",
            &none,
            1,
            false,
            &policy,
        ));
        assert!(!should_force_factual_tool_retry(
            TaskExecutionProfile::default(),
            "最新的一个ci?",
            &none,
            0,
            true,
            &policy,
        ));
        // Analysis tasks now also allow factual retry
        assert!(should_force_factual_tool_retry(
            infer_task_execution_profile("review 最新的commit"),
            "最新的一个ci?",
            &none,
            0,
            false,
            &policy,
        ));
        assert!(!should_force_factual_tool_retry(
            TaskExecutionProfile::default(),
            "hello",
            &none,
            0,
            false,
            &policy,
        ));
    }

    #[test]
    fn workspace_queries_detected_as_factual() {
        // Any query about workspace state should be detected as needing tools
        assert!(looks_like_factual_query("review local changes"));
        assert!(looks_like_factual_query("what changed"));
        assert!(looks_like_factual_query("评审当前修改"));
        assert!(looks_like_factual_query("看改动"));
        assert!(looks_like_factual_query("diff the code"));
        assert!(looks_like_factual_query("what's in this repo"));
        assert!(looks_like_factual_query("这个项目有什么"));
        // Generic non-workspace queries should NOT trigger
        assert!(!looks_like_factual_query("hello"));
        assert!(!looks_like_factual_query("explain quicksort"));
        assert!(!looks_like_factual_query("write a poem"));
    }

    #[test]
    fn analysis_tasks_allow_factual_retry() {
        let profile = infer_task_execution_profile("review local changes");
        assert!(profile.allow_factual_retry);
        let profile2 = infer_task_execution_profile("explain this function");
        assert!(profile2.allow_factual_retry);
    }

    #[test]
    fn contextual_live_query_detects_short_followup() {
        let recent = vec!["github_ci_status".to_string()];
        assert!(looks_like_live_query_with_context("最新的", &recent));
        assert!(looks_like_live_query_with_context("pr呢？", &recent));
        assert!(!looks_like_live_query_with_context("hello", &recent));
    }

    #[test]
    fn factual_retry_message_guides_toward_dedicated_tools() {
        let policy = TurnInteractionPolicy::from_visible_tool_names(
            TurnInteractionMode::Deny,
            vec!["mo_query".into()],
        );
        let msg = factual_tool_retry_message("memoria 最新的一个ci?", &policy);
        assert!(msg.contains("mo_query"));
        assert!(msg.contains("cannot pause for user clarification"));
        assert!(msg.contains("Discard your previous draft"));
    }

    #[test]
    fn factual_retry_message_lists_selected_tools_when_provided() {
        let policy = TurnInteractionPolicy::from_visible_tool_names(
            TurnInteractionMode::Deny,
            vec!["mo_query".to_string(), "read_file".to_string()],
        );
        let msg = factual_tool_retry_message("看session指标", &policy);
        assert!(msg.contains("mo_query"));
        assert!(msg.contains("read_file"));
        assert!(msg.contains("Only call tools from this list"));
    }

    #[test]
    fn factual_retry_message_mentions_ask_user_without_listing_it_as_evidence() {
        let policy = TurnInteractionPolicy::from_visible_tool_names(
            TurnInteractionMode::Prompt,
            vec!["mo_query".to_string(), "ask_user".to_string()],
        );
        let msg = factual_tool_retry_message("看当前 session 指标", &policy);
        assert!(msg.contains("mo_query"));
        assert!(msg.contains("ask_user"));
        assert!(!msg.contains("ask_user,"));
    }

    #[test]
    fn openai_factual_tool_retry_user_message_shape() {
        let policy = TurnInteractionPolicy::from_visible_tool_names(
            TurnInteractionMode::Deny,
            vec!["mo_query".into()],
        );
        let v = openai_factual_tool_retry_user_message("q", &policy);
        assert_eq!(v["role"], "user");
        let s = v["content"].as_str().unwrap();
        assert!(s.contains("Runtime correction"));
        assert!(s.contains("Original user query: q"));
    }

    #[test]
    fn factual_retry_requires_evidence_tools() {
        let policy = TurnInteractionPolicy::from_visible_tool_names(
            TurnInteractionMode::Prompt,
            vec!["ask_user".into()],
        );
        assert!(!should_force_factual_tool_retry(
            TaskExecutionProfile::default(),
            "latest CI?",
            &[],
            0,
            false,
            &policy,
        ));
    }

    #[test]
    fn session_not_found_detection() {
        assert!(is_session_not_found_error("Session not found"));
        assert!(is_session_not_found_error("error: SESSION NOT FOUND"));
        assert!(!is_session_not_found_error("authentication failed"));
        assert!(!is_session_not_found_error(""));
    }

    #[test]
    fn extract_repos_explicit_owner_repo() {
        let text = "user follows matrixorigin/Memoria and wants to track their projects";
        let repos = extract_repos_from_memory(text);
        assert_eq!(repos, vec!["matrixorigin/Memoria"]);
    }

    #[test]
    fn extract_repos_multiple() {
        let text = "tracks matrixorigin/Memoria and also watches rust-lang/rust";
        let repos = extract_repos_from_memory(text);
        assert_eq!(repos.len(), 2);
        assert!(repos.contains(&"matrixorigin/Memoria".to_string()));
        assert!(repos.contains(&"rust-lang/rust".to_string()));
    }

    #[test]
    fn extract_repos_dedup() {
        let text = "matrixorigin/Memoria and MATRIXORIGIN/memoria again";
        let repos = extract_repos_from_memory(text);
        assert_eq!(repos.len(), 1, "should deduplicate case-insensitively");
    }

    #[test]
    fn extract_repos_skips_tag_namespaces() {
        let text = "[@pref/active] user follows matrixorigin/Memoria";
        let repos = extract_repos_from_memory(text);
        assert_eq!(repos, vec!["matrixorigin/Memoria"]);
        assert!(
            !repos.iter().any(|r| r.contains("pref")),
            "should not extract @pref/active as a repo"
        );
    }

    #[test]
    fn extract_repos_skips_protocols() {
        let text = "see https://github.com/matrixorigin/Memoria for details";
        let repos = extract_repos_from_memory(text);
        assert!(repos.iter().any(|r| r == "matrixorigin/Memoria"));
        assert!(!repos.iter().any(|r| r.to_lowercase().contains("http")));
    }

    #[test]
    fn extract_repos_empty_for_no_repos() {
        let text = "user prefers concise responses and dark mode";
        let repos = extract_repos_from_memory(text);
        assert!(repos.is_empty());
    }

    #[test]
    fn extract_repos_handles_hyphen() {
        let text = "watching my-org/my-project and also some-user/cool-lib";
        let repos = extract_repos_from_memory(text);
        assert!(repos.iter().any(|r| r == "my-org/my-project"));
        assert!(repos.iter().any(|r| r == "some-user/cool-lib"));
    }
}
