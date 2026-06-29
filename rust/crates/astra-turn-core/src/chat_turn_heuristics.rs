//! Factual-query detection, session error classification, and memory repo extraction.
//! Shared by CLI `chat_stream` / `repl_turn` and available for in-process bridge parity tests.

use std::collections::HashSet;
use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;
use tracing::warn;

use crate::chat_history_openai::openai_user_content_message;
use crate::interaction_types::{ASK_USER_TOOL_NAME, TurnInteractionPolicy};

const DEFAULT_STALL_WINDOW: usize = 3;
const DEFAULT_EXPLORATION_ROUND_WINDOW: usize = 5;
const EXPLORATORY_STALL_WINDOW: usize = 4;
const EXPLORATORY_ROUND_WINDOW: usize = 8;

const STANDARD_ANALYSIS_TURN_BUDGET: AgenticTurnBudget = AgenticTurnBudget::new(60, 120, 20, 3);
const COMPLEX_ANALYSIS_TURN_BUDGET: AgenticTurnBudget = AgenticTurnBudget::new(90, 180, 30, 3);
const STANDARD_IMPLEMENTATION_TURN_BUDGET: AgenticTurnBudget =
    AgenticTurnBudget::new(80, 160, 20, 4);
const COMPLEX_IMPLEMENTATION_TURN_BUDGET: AgenticTurnBudget =
    AgenticTurnBudget::new(120, 240, 30, 4);
const STANDARD_EXPLORATORY_TURN_BUDGET: AgenticTurnBudget = AgenticTurnBudget::new(90, 180, 30, 3);
const COMPLEX_EXPLORATORY_TURN_BUDGET: AgenticTurnBudget = AgenticTurnBudget::new(120, 240, 30, 4);
const STANDARD_MUTATING_EXPLORATORY_TURN_BUDGET: AgenticTurnBudget =
    AgenticTurnBudget::new(100, 200, 25, 4);
const COMPLEX_MUTATING_EXPLORATORY_TURN_BUDGET: AgenticTurnBudget =
    AgenticTurnBudget::new(140, 280, 35, 4);

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

const EXPLICIT_MUTATION_DIRECTIVE_TERMS: &[&str] = &[
    "fix",
    "fix issues",
    "fix failures",
    "implement",
    "write",
    "edit",
    "update code",
    "create",
    "add ",
    "remove",
    "delete",
    "patch",
    "refactor",
    "apply",
    "cleanup",
    "clean up",
    "修改代码",
    "修改文件",
    "重构",
    "实现",
    "新增",
    "删除",
    "更新代码",
    "更新文件",
    "修复",
    "修正",
    "清理",
    "合理采纳",
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
    pub exploration_round_window: usize,
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
            allow_factual_retry: false,
            exploration_round_window: DEFAULT_EXPLORATION_ROUND_WINDOW,
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
    let has_explicit_mutation_directive =
        contains_any_keyword(&q, EXPLICIT_MUTATION_DIRECTIVE_TERMS);
    let has_analysis = contains_any_keyword(&q, ANALYSIS_TERMS);
    let exploratory = contains_any_keyword(&q, EXPLORATION_TERMS);
    let complexity = if contains_any_keyword(&q, COMPLEXITY_TERMS) {
        TaskComplexity::Complex
    } else {
        TaskComplexity::Standard
    };
    let exploration_round_window = if exploratory {
        EXPLORATORY_ROUND_WINDOW
    } else {
        DEFAULT_EXPLORATION_ROUND_WINDOW
    };
    let stall_window = if exploratory || complexity == TaskComplexity::Complex {
        EXPLORATORY_STALL_WINDOW
    } else {
        DEFAULT_STALL_WINDOW
    };
    // When both mutating and analysis terms are present, an explicit mutation
    // directive wins ("review and fix", "评审并修复", "清理过时测试"). Otherwise
    // analysis wins for object descriptions such as "评审当前修改" / "review
    // current changes".
    let should_mutate = has_explicit_mutation_directive || has_mutating && !has_analysis;
    if should_mutate {
        TaskExecutionProfile {
            mutates_workspace: true,
            verification_required: true,
            allow_factual_retry: false,
            exploration_round_window,
            stall_window,
            complexity,
            exploratory_task: exploratory,
            agentic_turn_budget: default_agentic_turn_budget(true, exploratory, complexity),
        }
    } else if has_analysis {
        TaskExecutionProfile {
            mutates_workspace: false,
            verification_required: false,
            allow_factual_retry: false,
            exploration_round_window,
            stall_window,
            complexity,
            exploratory_task: exploratory,
            agentic_turn_budget: default_agentic_turn_budget(false, exploratory, complexity),
        }
    } else {
        TaskExecutionProfile {
            exploration_round_window,
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
    if requested_hard > hard_turn_limit {
        warn!(
            requested_hard,
            ceiling,
            hard_turn_limit,
            "agentic turn budget override clamped: hard_turn_limit reduced to runtime ceiling"
        );
    }
    let requested_initial = override_budget
        .and_then(|value| value.initial_turns)
        .unwrap_or(budget.initial_turns);
    let initial_turns = requested_initial.max(1).min(hard_turn_limit);
    if requested_initial > initial_turns {
        warn!(
            requested_initial,
            hard_turn_limit,
            initial_turns,
            "agentic turn budget override clamped: initial_turns reduced to hard_turn_limit"
        );
    }
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
    let has_explicit_mutation_directive =
        contains_any_keyword(&q, EXPLICIT_MUTATION_DIRECTIVE_TERMS);
    has_explicit_mutation_directive || has_mutating && !has_read_only
}

pub fn should_force_factual_tool_retry(
    profile: TaskExecutionProfile,
    _input: &str,
    _recent_tools: &[String],
    total_evidence_tool_calls: u32,
    already_retried: bool,
    policy: &TurnInteractionPolicy,
) -> bool {
    profile.allow_factual_retry
        && !already_retried
        && total_evidence_tool_calls == 0
        && policy.has_evidence_tools()
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
        that you cannot know from training data alone. Validate the answer with tools first.\n\
        \n\
        {tools_hint}\n\
        {clarification_hint}\n\
        \n\
        If the available tools do not provide relevant evidence for the user's actual question, \
        answer the question directly instead of writing task-progress commentary.\n\
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

// ── Shared prompt text normalization ───────────────────────────────────────
//
/// Trim trailing punctuation, ellipsis markers, and Chinese tone particles
/// from a user message.
pub fn trim_trailing_punctuation(s: &str) -> &str {
    s.trim().trim_end_matches(|ch: char| {
        matches!(
            ch,
            '?' | '？'
                | '!'
                | '！'
                | '.'
                | '。'
                | ','
                | '，'
                | '啊'
                | '呀'
                | '呢'
                | '吧'
                | '嘛'
                | '啦'
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interaction_types::TurnInteractionMode;

    #[test]
    fn mutating_task_detection_distinguishes_read_only_flows() {
        assert!(!looks_like_mutating_task("review 最新的commit"));
        assert!(!looks_like_mutating_task(
            "what changed in the latest commit?"
        ));
        assert!(!looks_like_mutating_task("explain this diff"));
        assert!(!looks_like_mutating_task("评审当前修改"));
        assert!(!looks_like_mutating_task("审查修改"));
        assert!(!looks_like_mutating_task("看一下修改"));
        // Pure mutating without analysis context
        assert!(looks_like_mutating_task("implement the feature"));
        assert!(looks_like_mutating_task("fix the bug"));
        assert!(looks_like_mutating_task("修复这个问题"));
        // Mixed review + explicit mutation should still get a mutating profile.
        assert!(looks_like_mutating_task(
            "review the latest commit and fix any issues"
        ));
        assert!(looks_like_mutating_task(
            "review the current changes and fix any issues"
        ));
        assert!(looks_like_mutating_task(
            "多角度review这个分支changes，清理过时测试"
        ));
    }

    #[test]
    fn task_execution_profile_scales_budget_for_implementation_and_exploration() {
        let review = infer_task_execution_profile("review 最新的commit");
        assert!(!review.mutates_workspace);
        assert!(!review.verification_required);
        assert!(!review.allow_factual_retry);
        assert_eq!(review.stall_window, DEFAULT_STALL_WINDOW);

        let implementation = infer_task_execution_profile("implement the feature");
        assert!(implementation.mutates_workspace);
        assert!(implementation.verification_required);
        assert!(!implementation.allow_factual_retry);
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
        assert!(exploratory.exploration_round_window > review.exploration_round_window);
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
    fn explicit_mutation_wins_over_analysis_when_both_present() {
        // "review and fix" is a mixed request: inspect first, then mutate.
        let profile = infer_task_execution_profile("review the code and fix issues");
        assert!(profile.verification_required);
        assert!(profile.mutates_workspace);
        let profile = infer_task_execution_profile("多角度review这个分支changes，清理过时测试");
        assert!(profile.verification_required);
        assert!(profile.mutates_workspace);
        // Describing changes as the object of review remains read-only.
        let profile = infer_task_execution_profile("review current changes");
        assert!(!profile.verification_required);
        // Pure mutating still works
        let profile = infer_task_execution_profile("fix the compilation error");
        assert!(profile.verification_required);
    }

    #[test]
    fn force_retry_requires_explicit_profile_signal() {
        let none: Vec<String> = vec![];
        let policy = TurnInteractionPolicy::from_visible_tool_names(
            TurnInteractionMode::Deny,
            vec!["github".into()],
        );
        assert!(!should_force_factual_tool_retry(
            TaskExecutionProfile::default(),
            "最新的一个ci?",
            &none,
            0,
            false,
            &policy,
        ));

        let explicit = TaskExecutionProfile {
            allow_factual_retry: true,
            ..TaskExecutionProfile::default()
        };
        assert!(should_force_factual_tool_retry(
            explicit, "hello", &none, 0, false, &policy,
        ));
        assert!(!should_force_factual_tool_retry(
            explicit, "hello", &none, 1, false, &policy,
        ));
        assert!(!should_force_factual_tool_retry(
            explicit, "hello", &none, 0, true, &policy,
        ));
    }

    #[test]
    fn heuristic_profiles_do_not_enable_factual_retry() {
        let profile = infer_task_execution_profile("review local changes");
        assert!(!profile.allow_factual_retry);
        let profile2 = infer_task_execution_profile("explain this function");
        assert!(!profile2.allow_factual_retry);
        let profile3 = infer_task_execution_profile("implement the feature");
        assert!(!profile3.allow_factual_retry);
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
        assert!(msg.contains("If the available tools do not provide relevant evidence"));
        assert!(!msg.contains("Discard your previous draft"));
    }

    #[test]
    fn factual_retry_message_lists_visible_tools_when_provided() {
        let policy = TurnInteractionPolicy::from_visible_tool_names(
            TurnInteractionMode::Deny,
            vec![
                "mo_query".to_string(),
                "introspect".to_string(),
                "read_file".to_string(),
            ],
        );
        let msg = factual_tool_retry_message("看session指标", &policy);
        assert!(msg.contains("mo_query"));
        assert!(msg.contains("read_file"));
        assert!(!msg.contains("introspect"));
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
        let explicit = TaskExecutionProfile {
            allow_factual_retry: true,
            ..TaskExecutionProfile::default()
        };
        assert!(!should_force_factual_tool_retry(
            explicit,
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

    // ── trim_trailing_punctuation ────────────────────────────────────────

    #[test]
    fn trim_trailing_punctuation_removes_chinese_tone_particles() {
        assert_eq!(trim_trailing_punctuation("继续吧"), "继续");
        assert_eq!(trim_trailing_punctuation("好的呢"), "好的");
        assert_eq!(trim_trailing_punctuation("行啊"), "行");
    }

    #[test]
    fn trim_trailing_punctuation_removes_trailing_punctuation() {
        assert_eq!(trim_trailing_punctuation("继续？"), "继续");
        assert_eq!(trim_trailing_punctuation("继续！"), "继续");
        assert_eq!(trim_trailing_punctuation("go."), "go");
    }

    #[test]
    fn trim_trailing_punctuation_preserves_non_trailing() {
        assert_eq!(trim_trailing_punctuation("继续啊呀"), "继续");
        assert_eq!(trim_trailing_punctuation("next step"), "next step");
    }
}
