/// Tool idempotency classification — determines retry safety.
///
/// Shared across `astra-pipeline` (retry policies) and `astra-turn-core`
/// (central tool registry) via `astra-turn-types`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolIdempotency {
    /// Safe to re-execute (no side effects): read_file, grep, git_log, etc.
    PureRead,
    /// Overwrite-style write (safe if file unchanged): write_file
    IdempotentWrite,
    /// Must check cache, never blindly re-execute: bash, github_create_issue
    NonIdempotent,
}

impl ToolIdempotency {
    pub fn is_safe_to_retry(self) -> bool {
        matches!(self, Self::PureRead | Self::IdempotentWrite)
    }

    pub fn is_pure_read(self) -> bool {
        matches!(self, Self::PureRead)
    }
}

/// Canonical name → idempotency classification.
///
/// This is the single source of truth for tool idempotency, used by both
/// `tool_categories::ToolMeta` and `step_protocol::tool_retry_policy`.
pub fn classify_tool_idempotency(tool_name: &str) -> ToolIdempotency {
    match tool_name {
        // Pure read tools — safe to re-execute
        "read_file" | "file_read" | "ReadFileTool" | "get_file_contents" | "view_file" | "grep"
        | "GrepTool" | "glob" | "GlobTool" | "list_dir" | "ListDirTool" | "list_files"
        | "find_files" | "search_code" | "git_status" | "git_log" | "git_diff" | "git_show"
        | "git_blame" | "git_file_history" | "git_contributors" | "git_log_search" | "symbols"
        | "find_definition" | "find_references" | "symbol_search" | "hover_info" | "call_graph"
        | "type_hierarchy" | "dead_code" | "extract_members" | "github_list_prs"
        | "github_get_pr" | "github_ci_status" | "github_list_issues" | "github_get_issue"
        | "github_repo_stats" | "web_fetch" | "WebFetchTool" | "web_search" | "WebSearchTool"
        | "memory_search" | "memory_retrieve" | "memory_profile" | "mo_query"
        | "get_agent_info" | "get_agent_result" | "reflect" | "context_analysis" | "diagnose" | "search" | "find"
        | "tool_search" | "lsp" | "task_list" | "task_get" | "skill" | "discover_skills"
        | "brief" | "query_context" => ToolIdempotency::PureRead,

        // ask_user blocks until user responds — retrying it would double-prompt.
        // sleep has wall-clock side effects — not safe to retry transparently.
        "ask_user" | "sleep" => ToolIdempotency::NonIdempotent,

        // Idempotent writes — overwrite semantics
        "write_file" | "WriteFileTool" => ToolIdempotency::IdempotentWrite,

        // Everything else: non-idempotent (safe default)
        _ => ToolIdempotency::NonIdempotent,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pure_read_tools() {
        for name in [
            "read_file",
            "grep",
            "glob",
            "list_dir",
            "git_status",
            "git_log",
            "git_diff",
            "git_blame",
            "git_file_history",
            "git_contributors",
            "git_log_search",
            "github_list_prs",
            "github_get_pr",
            "github_list_issues",
            "github_get_issue",
            "github_ci_status",
            "github_repo_stats",
            "mo_query",
            "memory_search",
            "memory_profile",
            "web_fetch",
            "get_agent_info",
            "reflect",
        ] {
            assert_eq!(
                classify_tool_idempotency(name),
                ToolIdempotency::PureRead,
                "Expected PureRead for {name}"
            );
        }
    }

    #[test]
    fn idempotent_write() {
        assert_eq!(
            classify_tool_idempotency("write_file"),
            ToolIdempotency::IdempotentWrite
        );
        assert_eq!(
            classify_tool_idempotency("WriteFileTool"),
            ToolIdempotency::IdempotentWrite
        );
    }

    #[test]
    fn ask_user_and_sleep_are_non_idempotent() {
        // ask_user has a side effect: it blocks until user responds.
        // Retrying it would double-prompt the user — must NOT be PureRead.
        assert_eq!(
            classify_tool_idempotency("ask_user"),
            ToolIdempotency::NonIdempotent,
            "ask_user must be NonIdempotent — retrying double-prompts the user"
        );
        assert!(!classify_tool_idempotency("ask_user").is_safe_to_retry());

        // sleep has a wall-clock side effect and is not safe to retry transparently.
        assert_eq!(
            classify_tool_idempotency("sleep"),
            ToolIdempotency::NonIdempotent,
            "sleep must be NonIdempotent — wall-clock side effect"
        );
        assert!(!classify_tool_idempotency("sleep").is_safe_to_retry());
    }

    #[test]
    fn non_idempotent_tools() {
        for name in [
            "bash",
            "str_replace",
            "github_create_issue",
            "memory_store",
            "memory_purge",
            "memory_correct",
            "delete_file",
            "multi_edit",
            "edit_file",
            "ask_user",
            "sleep",
        ] {
            assert_eq!(
                classify_tool_idempotency(name),
                ToolIdempotency::NonIdempotent,
                "Expected NonIdempotent for {name}"
            );
        }
    }

    #[test]
    fn unknown_tool_defaults_to_non_idempotent() {
        assert_eq!(
            classify_tool_idempotency("some_future_tool"),
            ToolIdempotency::NonIdempotent
        );
    }

    #[test]
    fn retry_safety() {
        assert!(ToolIdempotency::PureRead.is_safe_to_retry());
        assert!(ToolIdempotency::IdempotentWrite.is_safe_to_retry());
        assert!(!ToolIdempotency::NonIdempotent.is_safe_to_retry());
    }

    #[test]
    fn pure_read_check() {
        assert!(ToolIdempotency::PureRead.is_pure_read());
        assert!(!ToolIdempotency::IdempotentWrite.is_pure_read());
        assert!(!ToolIdempotency::NonIdempotent.is_pure_read());
    }

    #[test]
    fn aliases_match_canonical() {
        let pairs = [
            ("read_file", "file_read"),
            ("read_file", "ReadFileTool"),
            ("grep", "GrepTool"),
            ("glob", "GlobTool"),
            ("list_dir", "ListDirTool"),
            ("write_file", "WriteFileTool"),
            ("web_fetch", "WebFetchTool"),
        ];
        for (canonical, alias) in pairs {
            assert_eq!(
                classify_tool_idempotency(canonical),
                classify_tool_idempotency(alias),
                "{canonical} and {alias} should have same idempotency"
            );
        }
    }
}
