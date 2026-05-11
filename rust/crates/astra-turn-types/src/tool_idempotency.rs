use serde_json::Value;

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

/// Canonical (name, args) → idempotency classification.
///
/// Single source of truth. Most tools dispatch on `name` alone, but a few
/// consolidated tools (e.g. `memory`) carry an `action` field whose value
/// changes read/write semantics — those branches consult `args`.
///
/// Pass `args = None` when args are unavailable (e.g. static schema audits);
/// action-sensitive tools then return the conservative `NonIdempotent`.
pub fn classify_tool_idempotency(tool_name: &str, args: Option<&Value>) -> ToolIdempotency {
    match tool_name {
        // ── Consolidated `memory` tool: branch on the `action` field. ──
        //
        // `store` / `purge` / `correct` mutate state → NonIdempotent.
        // `retrieve` / `search` / `profile` / `feedback` are pure reads.
        // Unknown or absent action → conservative NonIdempotent.
        "memory" => match args.and_then(|a| a.get("action")).and_then(Value::as_str) {
            Some("retrieve") | Some("search") | Some("profile") | Some("feedback") => {
                ToolIdempotency::PureRead
            }
            _ => ToolIdempotency::NonIdempotent,
        },

        // Pure read tools — safe to re-execute
        "read_file" | "file_read" | "ReadFileTool" | "get_file_contents" | "view_file" | "grep"
        | "GrepTool" | "glob" | "GlobTool" | "list_dir" | "ListDirTool" | "list_files"
        | "find_files" | "search_code" | "git_status" | "git_log" | "git_diff" | "git_show"
        | "git_blame" | "git_file_history" | "git_contributors" | "git_log_search" | "symbols"
        | "find_definition" | "find_references" | "symbol_search" | "hover_info" | "call_graph"
        | "type_hierarchy" | "dead_code" | "extract_members" | "github_list_prs"
        | "github_get_pr" | "github_ci_status" | "github_list_issues" | "github_get_issue"
        | "github_repo_stats" | "web_fetch" | "WebFetchTool" | "web_search" | "WebSearchTool"
        | "mo_query" | "get_agent_info" | "get_agent_result" | "reflect" | "context_analysis"
        | "diagnose" | "search" | "find" | "tool_search" | "lsp" | "task_list" | "task_get"
        | "skill" | "discover_skills" | "brief" | "query_context" => ToolIdempotency::PureRead,

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
    use serde_json::json;

    fn classify(name: &str) -> ToolIdempotency {
        classify_tool_idempotency(name, None)
    }

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
            "web_fetch",
            "get_agent_info",
            "reflect",
        ] {
            assert_eq!(
                classify(name),
                ToolIdempotency::PureRead,
                "Expected PureRead for {name}"
            );
        }
    }

    #[test]
    fn memory_action_aware() {
        // Read actions
        for action in ["retrieve", "search", "profile", "feedback"] {
            assert_eq!(
                classify_tool_idempotency("memory", Some(&json!({ "action": action }))),
                ToolIdempotency::PureRead,
                "memory(action={action}) should be PureRead"
            );
        }
        // Write actions
        for action in ["store", "purge", "correct"] {
            assert_eq!(
                classify_tool_idempotency("memory", Some(&json!({ "action": action }))),
                ToolIdempotency::NonIdempotent,
                "memory(action={action}) should be NonIdempotent"
            );
        }
        // Missing args → conservative
        assert_eq!(classify("memory"), ToolIdempotency::NonIdempotent);
        // Unknown action → conservative
        assert_eq!(
            classify_tool_idempotency("memory", Some(&json!({ "action": "nuke" }))),
            ToolIdempotency::NonIdempotent
        );
    }

    #[test]
    fn idempotent_write() {
        assert_eq!(classify("write_file"), ToolIdempotency::IdempotentWrite);
        assert_eq!(classify("WriteFileTool"), ToolIdempotency::IdempotentWrite);
    }

    #[test]
    fn ask_user_and_sleep_are_non_idempotent() {
        // ask_user has a side effect: it blocks until user responds.
        // Retrying it would double-prompt the user — must NOT be PureRead.
        assert_eq!(
            classify("ask_user"),
            ToolIdempotency::NonIdempotent,
            "ask_user must be NonIdempotent — retrying double-prompts the user"
        );
        assert!(!classify("ask_user").is_safe_to_retry());

        // sleep has a wall-clock side effect and is not safe to retry transparently.
        assert_eq!(
            classify("sleep"),
            ToolIdempotency::NonIdempotent,
            "sleep must be NonIdempotent — wall-clock side effect"
        );
        assert!(!classify("sleep").is_safe_to_retry());
    }

    #[test]
    fn non_idempotent_tools() {
        for name in [
            "bash",
            "str_replace",
            "github_create_issue",
            "delete_file",
            "multi_edit",
            "edit_file",
            "ask_user",
            "sleep",
        ] {
            assert_eq!(
                classify(name),
                ToolIdempotency::NonIdempotent,
                "Expected NonIdempotent for {name}"
            );
        }
    }

    #[test]
    fn unknown_tool_defaults_to_non_idempotent() {
        assert_eq!(classify("some_future_tool"), ToolIdempotency::NonIdempotent);
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
                classify(canonical),
                classify(alias),
                "{canonical} and {alias} should have same idempotency"
            );
        }
    }
}
