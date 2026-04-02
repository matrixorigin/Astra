//! Factual-query detection, session error classification, and memory repo extraction.
//! Shared by CLI `chat_stream` / `repl_turn` and available for in-process bridge parity tests.

use std::collections::HashSet;
use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

use super::chat_history_openai::openai_user_content_message;

const DEFAULT_STALL_WINDOW: usize = 3;
const DEFAULT_EXPLORATION_ROUND_BUDGET: usize = 5;
const ANALYSIS_STALL_WINDOW: usize = 4;
const ANALYSIS_EXPLORATION_ROUND_BUDGET: usize = 8;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TaskExecutionProfile {
    pub mutates_workspace: bool,
    pub verification_required: bool,
    pub allow_factual_retry: bool,
    pub exploration_round_budget: usize,
    pub stall_window: usize,
}

impl Default for TaskExecutionProfile {
    fn default() -> Self {
        Self {
            mutates_workspace: false,
            verification_required: false,
            allow_factual_retry: true,
            exploration_round_budget: DEFAULT_EXPLORATION_ROUND_BUDGET,
            stall_window: DEFAULT_STALL_WINDOW,
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
    // When both mutating and analysis terms are present, analysis wins —
    // the user is asking to *review/inspect* something that involves changes,
    // not asking to *make* changes. E.g. "评审当前修改" = review changes.
    if has_mutating && !has_analysis {
        TaskExecutionProfile {
            mutates_workspace: true,
            verification_required: true,
            allow_factual_retry: true,
            exploration_round_budget: DEFAULT_EXPLORATION_ROUND_BUDGET,
            stall_window: DEFAULT_STALL_WINDOW,
        }
    } else if has_analysis {
        TaskExecutionProfile {
            mutates_workspace: false,
            verification_required: false,
            allow_factual_retry: true,
            exploration_round_budget: ANALYSIS_EXPLORATION_ROUND_BUDGET,
            stall_window: ANALYSIS_STALL_WINDOW,
        }
    } else if has_mutating {
        // Mutating without analysis context
        TaskExecutionProfile {
            mutates_workspace: true,
            verification_required: true,
            allow_factual_retry: true,
            exploration_round_budget: DEFAULT_EXPLORATION_ROUND_BUDGET,
            stall_window: DEFAULT_STALL_WINDOW,
        }
    } else {
        TaskExecutionProfile::default()
    }
}

/// Cloud API returned no such session (case-insensitive substring match).
pub fn is_session_not_found_error(error: &str) -> bool {
    error.to_lowercase().contains("session not found")
}

/// Detect queries that almost certainly need tool calls to answer correctly.
/// Used for the hallucination guard: if LLM answers these with 0 tool calls,
/// the response is likely fabricated.
pub fn looks_like_factual_query(input: &str) -> bool {
    let q = input.to_lowercase();
    let github_keywords = [
        "pr",
        "pull request",
        "issue",
        "拉取请求",
        "问题",
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
        "看一下",
        "看看",
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
    total_tool_calls: u32,
    already_retried: bool,
) -> bool {
    profile.allow_factual_retry
        && !already_retried
        && total_tool_calls == 0
        && looks_like_live_query_with_context(input, recent_tools)
}

pub fn factual_tool_retry_message(original_query: &str) -> String {
    format!(
        "Runtime correction: your previous response answered without using any tools. \
This query requires live data from the workspace, repository, or external sources \
that you cannot know from training data alone. Retry from scratch and call tools first.\n\
\n\
- For workspace state: git_status, git_diff, read_file, grep, glob.\n\
- For GitHub data: github_ci_status, github_list_prs, github_list_issues, github_repo_stats.\n\
- For memory: memory_search, memory_profile.\n\
- Prefer dedicated tools over bash.\n\
\n\
Discard your previous draft and gather evidence with tools before answering.\n\
\n\
Original user query: {original_query}"
    )
}

/// OpenAI `messages` entry for [`factual_tool_retry_message`].
#[must_use]
pub fn openai_factual_tool_retry_user_message(original_query: &str) -> Value {
    openai_user_content_message(&factual_tool_retry_message(original_query))
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
    fn task_execution_profile_relaxes_analysis_loops() {
        let profile = infer_task_execution_profile("review 最新的commit");
        assert!(!profile.mutates_workspace);
        assert!(!profile.verification_required);
        assert!(profile.allow_factual_retry);
        assert_eq!(profile.stall_window, ANALYSIS_STALL_WINDOW);
        assert_eq!(
            profile.exploration_round_budget,
            ANALYSIS_EXPLORATION_ROUND_BUDGET
        );

        let profile = infer_task_execution_profile("implement the feature");
        assert!(profile.mutates_workspace);
        assert!(profile.verification_required);
        assert!(profile.allow_factual_retry);
        assert_eq!(profile.stall_window, DEFAULT_STALL_WINDOW);
        assert_eq!(
            profile.exploration_round_budget,
            DEFAULT_EXPLORATION_ROUND_BUDGET
        );
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
    fn force_retry_only_for_first_zero_tool_factual_answer() {
        let none: Vec<String> = vec![];
        assert!(should_force_factual_tool_retry(
            TaskExecutionProfile::default(),
            "最新的一个ci?",
            &none,
            0,
            false
        ));
        assert!(!should_force_factual_tool_retry(
            TaskExecutionProfile::default(),
            "最新的一个ci?",
            &none,
            1,
            false
        ));
        assert!(!should_force_factual_tool_retry(
            TaskExecutionProfile::default(),
            "最新的一个ci?",
            &none,
            0,
            true
        ));
        // Analysis tasks now also allow factual retry
        assert!(should_force_factual_tool_retry(
            infer_task_execution_profile("review 最新的commit"),
            "最新的一个ci?",
            &none,
            0,
            false
        ));
        assert!(!should_force_factual_tool_retry(
            TaskExecutionProfile::default(),
            "hello",
            &none,
            0,
            false
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
        let msg = factual_tool_retry_message("memoria 最新的一个ci?");
        assert!(msg.contains("github_ci_status"));
        assert!(msg.contains("git_status"));
        assert!(msg.contains("read_file"));
        assert!(msg.contains("memoria"));
        assert!(msg.contains("Discard your previous draft"));
    }

    #[test]
    fn openai_factual_tool_retry_user_message_shape() {
        let v = openai_factual_tool_retry_user_message("q");
        assert_eq!(v["role"], "user");
        let s = v["content"].as_str().unwrap();
        assert!(s.contains("Runtime correction"));
        assert!(s.contains("Original user query: q"));
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
