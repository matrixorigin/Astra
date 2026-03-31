//! Factual-query detection, session error classification, and memory repo extraction.
//! Shared by CLI `chat_stream` / `repl_turn` and available for in-process bridge parity tests.

use std::collections::HashSet;
use std::sync::LazyLock;

use regex::Regex;

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
    has_github || has_memory || has_git_live || has_code || has_web
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
    input: &str,
    recent_tools: &[String],
    total_tool_calls: u32,
    already_retried: bool,
) -> bool {
    !already_retried
        && total_tool_calls == 0
        && looks_like_live_query_with_context(input, recent_tools)
}

pub fn factual_tool_retry_message(original_query: &str) -> String {
    format!(
        "Runtime correction: your previous draft answered a live/factual query without using tools. Retry this turn from scratch and call at least one tool before answering.\n\
\n\
- For GitHub live data prefer github_ci_status / github_list_prs / github_list_issues / github_repo_stats.\n\
- For memory contents use memory_search or memory_profile.\n\
- For workspace change status use git_status or git_diff.\n\
- Do NOT fall back to bash when a dedicated GitHub or memory tool exists.\n\
- If repo was omitted before, infer it from the user's text or recent conversation. Bare names like 'memoria' and 'matrixone' are allowed.\n\
\n\
Original user query: {original_query}"
    )
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
            "最新的一个ci?",
            &none,
            0,
            false
        ));
        assert!(!should_force_factual_tool_retry(
            "最新的一个ci?",
            &none,
            1,
            false
        ));
        assert!(!should_force_factual_tool_retry(
            "最新的一个ci?",
            &none,
            0,
            true
        ));
        assert!(!should_force_factual_tool_retry("hello", &none, 0, false));
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
        assert!(msg.contains("github_repo_stats"));
        assert!(msg.contains("memoria"));
        assert!(msg.contains("Do NOT fall back to bash"));
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
