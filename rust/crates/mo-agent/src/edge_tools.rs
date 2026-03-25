//! Edge tool definitions and execution for the mo-agent CLI.
//!
//! Tools: bash, read_file, write_file, str_replace, list_dir, grep, glob,
//!        git_status, git_diff, git_log

use std::{
    env, fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};

use chrono::{DateTime, Utc};
use reqwest::{Client, Method, StatusCode};
use serde_json::{Value, json};

#[path = "edge_tools/fs.rs"]
mod fs_tools;
#[path = "edge_tools/github.rs"]
mod github;
#[path = "edge_tools/shell.rs"]
mod shell;

// ─── Tool schema ─────────────────────────────────────────────────────────────

pub fn all_tool_schemas() -> Vec<Value> {
    vec![
        json!({
            "type": "function",
            "function": {
                "name": "bash",
                "description": "Execute a shell command in the project root. Use for builds, tests, installs, git operations, or any CLI task. Can run curl to fetch URLs, call GitHub API, or access any network resource.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "command": {"type": "string", "description": "Shell command to run"},
                        "timeout": {"type": "number", "description": "Timeout in seconds (default 30)"}
                    },
                    "required": ["command"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "read_file",
                "description": "Read file contents. Use to inspect code, configs, or any text file.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "File path relative to project root"},
                        "start_line": {"type": "integer", "description": "First line to read (1-based, optional)"},
                        "end_line": {"type": "integer", "description": "Last line to read (inclusive, optional)"}
                    },
                    "required": ["path"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "write_file",
                "description": "Create or overwrite a file. Use str_replace to edit existing files.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "File path relative to project root"},
                        "content": {"type": "string", "description": "File content"}
                    },
                    "required": ["path", "content"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "str_replace",
                "description": "Replace an exact string in a file. old_str must match exactly (including whitespace). Use for targeted edits.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "File path relative to project root"},
                        "old_str": {"type": "string", "description": "Exact string to replace"},
                        "new_str": {"type": "string", "description": "Replacement string"}
                    },
                    "required": ["path", "old_str", "new_str"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "list_dir",
                "description": "List directory contents. Use to explore project structure or find files.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "Directory path (default: project root)"},
                        "depth": {"type": "integer", "description": "Max depth (default 1)"}
                    },
                    "required": []
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "grep",
                "description": "Search for a pattern in files. Returns matching lines with file:line context.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "pattern": {"type": "string", "description": "Regex pattern to search for"},
                        "path": {"type": "string", "description": "Directory or file to search (default: project root)"},
                        "include": {"type": "string", "description": "File glob filter e.g. '*.rs'"},
                        "case_sensitive": {"type": "boolean", "description": "Case sensitive (default false)"}
                    },
                    "required": ["pattern"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "glob",
                "description": "Find files matching a glob pattern.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "pattern": {"type": "string", "description": "Glob pattern e.g. '**/*.rs'"},
                        "path": {"type": "string", "description": "Root directory (default: project root)"}
                    },
                    "required": ["pattern"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "git_status",
                "description": "Show git working tree status.",
                "parameters": {"type": "object", "properties": {}, "required": []}
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "git_diff",
                "description": "Show git diff. Optionally diff against a ref or show staged changes.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "ref": {"type": "string", "description": "Git ref to diff against (optional)"},
                        "staged": {"type": "boolean", "description": "Show staged changes (default false)"}
                    },
                    "required": []
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "git_log",
                "description": "Show recent git commits in the LOCAL repository.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "n": {"type": "integer", "description": "Number of commits (default 10)"}
                    },
                    "required": []
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "github_list_prs",
                "description": "List pull requests from a GitHub repository. Use for GitHub PRs, recent changes, what's in review, or latest PRs. repo can be 'owner/repo' or a bare project name that will be auto-resolved when safe. detail: 'brief' (default) returns number/title/author/state/created_at; 'normal' adds body summary + labels + reviewers; 'detailed' adds review counts and merge metadata; 'full' keeps the same fields with larger truncation budgets. If resolved_by_search=true in the result, show results first and note which repo was used.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "repo": {"type": "string", "description": "Repository as 'owner/repo' or bare project name"},
                        "state": {"type": "string", "description": "PR state: 'open' (default), 'closed', or 'all'"},
                        "limit": {"type": "integer", "description": "Max PRs to return (default depends on detail)"},
                        "detail": {"type": "string", "enum": ["brief", "normal", "detailed", "full"], "description": "Output detail level: brief (default), normal, detailed, or full"}
                    },
                    "required": ["repo"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "github_get_pr",
                "description": "Get details of a specific GitHub PR. repo can be 'owner/repo' or a bare project name that will be auto-resolved when safe. detail: 'brief' (default) returns number/title/author/state/created_at/ci_conclusion; 'normal' adds body summary + labels + reviewers + changed_files; 'detailed' adds additions/deletions, key changed files, review comment count, and merge metadata; 'full' adds full body, per-file diff summaries, and review comments. If resolved_by_search=true in the result, show results first and note which repo was used.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "repo": {"type": "string", "description": "Repository as 'owner/repo' or bare project name"},
                        "pr_number": {"type": "integer", "description": "PR number"},
                        "detail": {"type": "string", "enum": ["brief", "normal", "detailed", "full"], "description": "Output detail level: brief (default), normal, detailed, or full"}
                    },
                    "required": ["repo", "pr_number"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "github_ci_status",
                "description": "Check CI/CD workflow runs for a GitHub repository. Use for CI, build status, test results, or workflow runs. repo can be 'owner/repo' or a bare project name that will be auto-resolved when safe. detail: 'brief' (default) returns workflow name/conclusion/branch/triggered_at; 'normal' adds PR info, commit message, and duration; 'detailed' adds failed jobs and first failed steps; 'full' adds all job statuses and more failed-step detail. If resolved_by_search=true in the result, show results first and note which repo was used.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "repo": {"type": "string", "description": "Repository as 'owner/repo' or bare project name"},
                        "limit": {"type": "integer", "description": "Max workflow runs to return (default 1)"},
                        "detail": {"type": "string", "enum": ["brief", "normal", "detailed", "full"], "description": "Output detail level: brief (default), normal, detailed, or full"}
                    },
                    "required": ["repo"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "github_list_issues",
                "description": "List issues from a GitHub repository. Use for bugs, feature requests, open issues, or GitHub issues. repo can be 'owner/repo' or a bare project name that will be auto-resolved when safe. detail: 'brief' (default) returns number/title/state/labels/created_at; 'normal' adds body + assignee + milestone + comment_count; 'detailed' and 'full' keep the same list fields with larger truncation budgets and are best paired with github_get_issue for comment-level detail. If resolved_by_search=true in the result, show results first and note which repo was used.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "repo": {"type": "string", "description": "Repository as 'owner/repo' or bare project name"},
                        "state": {"type": "string", "description": "Issue state: 'open' (default), 'closed', or 'all'"},
                        "labels": {"type": "string", "description": "Comma-separated label names to filter by"},
                        "limit": {"type": "integer", "description": "Max issues to return (default depends on detail)"},
                        "detail": {"type": "string", "enum": ["brief", "normal", "detailed", "full"], "description": "Output detail level: brief (default), normal, detailed, or full"}
                    },
                    "required": ["repo"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "github_get_issue",
                "description": "Get details of a specific GitHub issue. repo can be 'owner/repo' or a bare project name that will be auto-resolved when safe. detail: 'brief' (default) returns number/title/state/labels/created_at; 'normal' adds body + assignee + milestone + comment_count; 'detailed' adds recent comments; 'full' adds the full body and more comments. If resolved_by_search=true in the result, show results first and note which repo was used.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "repo": {"type": "string", "description": "Repository as 'owner/repo' or bare project name"},
                        "issue_number": {"type": "integer", "description": "Issue number"},
                        "detail": {"type": "string", "enum": ["brief", "normal", "detailed", "full"], "description": "Output detail level: brief (default), normal, detailed, or full"}
                    },
                    "required": ["repo", "issue_number"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "github_create_issue",
                "description": "Create a new GitHub issue. Use when user explicitly asks to create/file/open an issue. Requires repo in 'owner/repo' form and a configured GITHUB_TOKEN.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "repo": {"type": "string", "description": "Repository as 'owner/repo'"},
                        "title": {"type": "string", "description": "Issue title"},
                        "body": {"type": "string", "description": "Issue body (markdown)"},
                        "labels": {"type": "string", "description": "Comma-separated label names"}
                    },
                    "required": ["repo", "title"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "memory_store",
                "description": "Store a new memory. Use when user shares a fact, preference, decision, or anything worth remembering for future sessions.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "content": {"type": "string", "description": "Memory content to store"},
                        "memory_type": {"type": "string", "description": "Type: semantic (default), profile, procedural, working"}
                    },
                    "required": ["content"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "memory_search",
                "description": "Search memories by keyword or topic. Use when user asks 'what do you know about X'.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": {"type": "string", "description": "Search query"},
                        "top_k": {"type": "integer", "description": "Max results (default 10)"}
                    },
                    "required": ["query"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "memory_purge",
                "description": "Delete memories by topic keyword. Use when user asks to forget something.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "topic": {"type": "string", "description": "Keyword to match memories to delete"},
                        "reason": {"type": "string", "description": "Reason for deletion"}
                    },
                    "required": ["topic"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "get_agent_info",
                "description": "Query agent runtime diagnostics: token budget, context window breakdown, what model is being used, agent capabilities, available tools, memory retrieval scores (why a memory was or wasn't retrieved), context trend across turns. DO NOT use this for memory store/recall — use memory tools instead.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "dimension": {
                            "type": "string",
                            "enum": ["capability", "state", "memory", "identity", "context_snapshot", "context_trend", "all"],
                            "description": "Which dimension to query. 'context_snapshot': token usage for current turn. 'context_trend': growth across recent turns. 'capability': what tools/skills are available. 'state': current session state. 'all': everything."
                        }
                    },
                    "required": ["dimension"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "reflect",
                "description": "Diagnose past tool execution: why a tool failed, why results were unexpected, why a specific tool was or wasn't selected, performance bottlenecks. Analyzes the event trail from previous turns. Call this when asked to diagnose, debug, or explain past behavior.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "focus": {
                            "type": "string",
                            "enum": ["auto", "skill_failure", "unexpected_result", "data_quality", "tool_selection", "history", "performance"],
                            "description": "What to investigate."
                        },
                        "question": {
                            "type": "string",
                            "description": "Specific question to investigate, e.g. 'why wasn't list_prs used?'"
                        },
                        "last_n": {
                            "type": "integer",
                            "description": "How many recent events to analyze (default 20)"
                        }
                    },
                    "required": []
                }
            }
        }),
    ]
}

// ─── Tool execution ───────────────────────────────────────────────────────────

pub struct ToolExecutor {
    pub project_root: PathBuf,
    /// Cloud API base URL — used to proxy memory tool calls through the server
    /// so the server can add user_id for proper multi-user isolation.
    pub cloud_base: Option<String>,
    /// Auth token for cloud proxy calls.
    pub cloud_token: Option<String>,
    /// Optional GitHub token for authenticated GitHub API requests.
    pub github_token: Option<String>,
    /// Shared async GitHub client for edge tools.
    pub github_client: Client,
}

impl ToolExecutor {
    pub fn new(project_root: impl Into<PathBuf>) -> Self {
        Self {
            project_root: project_root.into(),
            cloud_base: None,
            cloud_token: None,
            github_token: env::var("GITHUB_TOKEN")
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty()),
            github_client: Client::builder()
                .timeout(Duration::from_secs(15))
                .user_agent(format!("mo-agent/{}", env!("CARGO_PKG_VERSION")))
                .build()
                .expect("failed to create GitHub HTTP client"),
        }
    }

    /// Configure cloud proxy for memory tool calls.
    pub fn with_cloud(mut self, base: impl Into<String>, token: impl Into<String>) -> Self {
        self.cloud_base = Some(base.into());
        self.cloud_token = Some(token.into());
        self
    }

    #[allow(dead_code)]
    pub fn with_github_token(mut self, token: impl Into<String>) -> Self {
        let token = token.into();
        let token = token.trim().to_string();
        self.github_token = if token.is_empty() { None } else { Some(token) };
        self
    }

    pub async fn execute(&self, name: &str, args: &Value) -> String {
        match name {
            "bash" => self.bash(args),
            "read_file" => self.read_file(args),
            "write_file" => self.write_file(args),
            "str_replace" => self.str_replace(args),
            "list_dir" => self.list_dir(args),
            "grep" => self.grep(args),
            "glob" => self.glob(args),
            "git_status" => self.git_run(&["status", "--short", "--branch"]),
            "git_diff" => self.git_diff(args),
            "git_log" => self.git_log(args),
            "github_list_prs" => self.github_list_prs(args).await,
            "github_get_pr" => self.github_get_pr(args).await,
            "github_ci_status" => self.github_ci_status(args).await,
            "github_list_issues" => self.github_list_issues(args).await,
            "github_get_issue" => self.github_get_issue(args).await,
            "github_create_issue" => self.github_create_issue(args).await,
            "memory_retrieve" => self.memoria_call("retrieve", args).await,
            "memory_store" => self.memoria_call("store", args).await,
            "memory_search" => self.memoria_call("search", args).await,
            "memory_purge" => self.memoria_call("purge", args).await,
            "get_agent_info" => {
                let dimension = args
                    .get("dimension")
                    .and_then(|v| v.as_str())
                    .unwrap_or("all");
                let info = match dimension {
                    "capability" => serde_json::json!({
                        "tools": self.tool_names(),
                        "tool_count": self.tool_count(),
                        "model": "see /model",
                        "note": "For full capability info including model/token budget, ask the server via /session"
                    }),
                    "state" => serde_json::json!({
                        "note": "Runtime state is managed by the edge CLI. Use /session for current session info."
                    }),
                    "context_snapshot" | "context_trend" => serde_json::json!({
                        "note": "Context window data is available from the server. Check the explain output (/explain) for token breakdown."
                    }),
                    "identity" => serde_json::json!({
                        "name": "mo-agent",
                        "version": env!("CARGO_PKG_VERSION"),
                        "runtime": "Rust edge CLI",
                        "note": "Cloud-side identity (model, system prompt) is server-managed."
                    }),
                    _ => serde_json::json!({
                        "tools_available": self.tool_names(),
                        "tool_count": self.tool_count(),
                        "runtime": "mo-agent Rust CLI",
                        "version": env!("CARGO_PKG_VERSION"),
                        "note": "For full agent info including memory, context, model details, the server provides richer data."
                    }),
                };
                info.to_string()
            }
            "reflect" => {
                let focus = args.get("focus").and_then(|v| v.as_str()).unwrap_or("auto");
                let question = args.get("question").and_then(|v| v.as_str()).unwrap_or("");
                let last_n = args.get("last_n").and_then(|v| v.as_i64()).unwrap_or(20);
                serde_json::json!({
                    "status": "reflect_requires_session",
                    "focus": focus,
                    "question": question,
                    "last_n": last_n,
                    "note": "Reflect data comes from the server API. Use /reflect command for direct access."
                }).to_string()
            }
            _ => format!("Unknown tool: {name}"),
        }
    }

    fn tool_names(&self) -> Vec<String> {
        all_tool_schemas()
            .iter()
            .filter_map(|s| {
                s.get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
            })
            .collect()
    }

    fn tool_count(&self) -> usize {
        all_tool_schemas().len()
    }

    async fn memoria_call(&self, op: &str, args: &Value) -> String {
        // Build endpoint and payload
        let (endpoint, payload, auth_header) = if let (Some(cloud_base), Some(token)) =
            (&self.cloud_base, &self.cloud_token)
        {
            (
                format!("{cloud_base}/memory/{op}"),
                args.clone(),
                format!("Bearer {token}"),
            )
        } else {
            let base = std::env::var("MEMORIA_BASE_URL")
                .unwrap_or_else(|_| "http://localhost:8100".to_string());
            let key = match std::env::var("MEMORIA_API_KEY")
                .ok()
                .or_else(|| std::env::var("MEMORIA_MASTER_KEY").ok())
            {
                Some(k) => k,
                None => {
                    return json!({
                            "error": "Memory unavailable: not connected to cloud and MEMORIA_API_KEY not set",
                            "hint": "Login with /login to enable cloud-backed memory with user isolation"
                        })
                        .to_string();
                }
            };

            let (ep, pl) = match op {
                "retrieve" => {
                    let query = args.get("query").and_then(Value::as_str).unwrap_or("");
                    let top_k = args.get("top_k").and_then(Value::as_u64).unwrap_or(5);
                    (
                        format!("{base}/v1/memories/retrieve"),
                        json!({"query": query, "top_k": top_k}),
                    )
                }
                "store" => {
                    let content = args.get("content").and_then(Value::as_str).unwrap_or("");
                    let memory_type = args
                        .get("memory_type")
                        .and_then(Value::as_str)
                        .unwrap_or("semantic");
                    (
                        format!("{base}/v1/memories"),
                        json!({"content": content, "memory_type": memory_type}),
                    )
                }
                "search" => {
                    let query = args.get("query").and_then(Value::as_str).unwrap_or("");
                    let top_k = args.get("top_k").and_then(Value::as_u64).unwrap_or(10);
                    (
                        format!("{base}/v1/memories/search"),
                        json!({"query": query, "top_k": top_k}),
                    )
                }
                "purge" => {
                    let topic = args.get("topic").and_then(Value::as_str).unwrap_or("");
                    let reason = args
                        .get("reason")
                        .and_then(Value::as_str)
                        .unwrap_or("user request");
                    (
                        format!("{base}/v1/memories/purge"),
                        json!({"topic": topic, "reason": reason}),
                    )
                }
                _ => return format!("Unknown memoria op: {op}"),
            };
            (ep, pl, format!("Bearer {key}"))
        };

        match reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .no_proxy()
            .build()
        {
            Ok(client) => match client
                .post(&endpoint)
                .header("Authorization", &auth_header)
                .json(&payload)
                .send()
                .await
            {
                Ok(resp) => match resp.text().await {
                    Ok(text) => text,
                    Err(e) => json!({"error": format!("read response: {e}")}).to_string(),
                },
                Err(e) => json!({"error": format!("memoria request failed: {e}")}).to_string(),
            },
            Err(e) => json!({"error": format!("build client: {e}")}).to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn test_executor() -> ToolExecutor {
        ToolExecutor::new(std::env::temp_dir())
    }

    // ── all_tool_schemas ──────────────────────────────────────────────────────

    #[test]
    fn all_tool_schemas_non_empty() {
        let schemas = all_tool_schemas();
        assert!(!schemas.is_empty(), "should have at least one tool schema");
    }

    #[test]
    fn all_tool_schemas_have_function_name() {
        for schema in all_tool_schemas() {
            let name = schema
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str());
            assert!(name.is_some(), "schema missing function.name: {schema}");
            assert!(!name.unwrap().is_empty());
        }
    }

    #[test]
    fn all_tool_schemas_have_description() {
        for schema in all_tool_schemas() {
            let desc = schema
                .get("function")
                .and_then(|f| f.get("description"))
                .and_then(|d| d.as_str());
            assert!(
                desc.is_some(),
                "schema missing description: {:?}",
                schema["function"]["name"]
            );
        }
    }

    #[test]
    fn tool_schemas_include_core_tools() {
        let names: Vec<String> = all_tool_schemas()
            .iter()
            .filter_map(|s| {
                s.get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|n| n.as_str())
                    .map(String::from)
            })
            .collect();
        for expected in &[
            "bash",
            "read_file",
            "write_file",
            "str_replace",
            "list_dir",
            "grep",
            "glob",
            "git_status",
            "github_ci_status",
            "memory_store",
            "memory_search",
            "reflect",
        ] {
            assert!(
                names.contains(&expected.to_string()),
                "missing tool: {expected}"
            );
        }
    }

    #[test]
    fn no_duplicate_tool_names() {
        let names: Vec<String> = all_tool_schemas()
            .iter()
            .filter_map(|s| {
                s.get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|n| n.as_str())
                    .map(String::from)
            })
            .collect();
        let mut seen = std::collections::HashSet::new();
        for name in &names {
            assert!(seen.insert(name), "duplicate tool name: {name}");
        }
    }

    // ── ToolExecutor ──────────────────────────────────────────────────────────

    #[test]
    fn executor_tool_count_matches_schemas() {
        let executor = test_executor();
        assert_eq!(executor.tool_count(), all_tool_schemas().len());
    }

    #[test]
    fn executor_tool_names_match_schemas() {
        let executor = test_executor();
        let names = executor.tool_names();
        assert_eq!(names.len(), all_tool_schemas().len());
        assert!(names.contains(&"bash".to_string()));
    }

    #[tokio::test]
    async fn execute_unknown_tool_returns_error() {
        let executor = test_executor();
        let result = executor.execute("nonexistent_tool", &json!({})).await;
        assert!(result.contains("Unknown tool"), "got: {result}");
    }

    #[tokio::test]
    async fn execute_reflect_returns_placeholder() {
        let executor = test_executor();
        let result = executor.execute("reflect", &json!({"focus": "auto"})).await;
        assert!(result.contains("reflect_requires_session"), "got: {result}");
    }

    // ── fs tools ──────────────────────────────────────────────────────────────

    #[test]
    fn resolve_absolute_path_unchanged() {
        let executor = test_executor();
        let resolved = executor.resolve("/tmp/test.txt");
        assert_eq!(resolved, PathBuf::from("/tmp/test.txt"));
    }

    #[test]
    fn resolve_relative_path_joins_project_root() {
        let executor = ToolExecutor::new("/my/project");
        let resolved = executor.resolve("src/main.rs");
        assert_eq!(resolved, PathBuf::from("/my/project/src/main.rs"));
    }

    #[test]
    fn read_file_missing_path_returns_error() {
        let executor = test_executor();
        let result = executor.read_file(&json!({}));
        assert!(result.contains("Error"), "got: {result}");
    }

    #[test]
    fn read_file_nonexistent_returns_error() {
        let executor = test_executor();
        let result = executor.read_file(&json!({"path": "/nonexistent/file.txt"}));
        assert!(result.contains("Error"), "got: {result}");
    }

    #[test]
    fn write_and_read_file_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let executor = ToolExecutor::new(dir.path());
        let path = "test_roundtrip.txt";

        let write_result = executor.write_file(&json!({"path": path, "content": "hello world"}));
        assert!(
            write_result.contains("Written"),
            "write failed: {write_result}"
        );

        let read_result = executor.read_file(&json!({"path": path}));
        assert_eq!(read_result, "hello world");
    }

    #[test]
    fn str_replace_works() {
        let dir = tempfile::tempdir().unwrap();
        let executor = ToolExecutor::new(dir.path());
        let path = "replace_test.txt";

        executor.write_file(&json!({"path": path, "content": "foo bar baz"}));
        let result =
            executor.str_replace(&json!({"path": path, "old_str": "bar", "new_str": "qux"}));
        assert!(result.contains("Replaced"), "got: {result}");

        let content = executor.read_file(&json!({"path": path}));
        assert_eq!(content, "foo qux baz");
    }

    #[test]
    fn str_replace_rejects_non_unique() {
        let dir = tempfile::tempdir().unwrap();
        let executor = ToolExecutor::new(dir.path());
        let path = "dup_test.txt";

        executor.write_file(&json!({"path": path, "content": "aaa aaa"}));
        let result =
            executor.str_replace(&json!({"path": path, "old_str": "aaa", "new_str": "bbb"}));
        assert!(result.contains("2 times"), "got: {result}");
    }

    #[test]
    fn str_replace_rejects_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let executor = ToolExecutor::new(dir.path());
        let path = "nf_test.txt";

        executor.write_file(&json!({"path": path, "content": "hello"}));
        let result =
            executor.str_replace(&json!({"path": path, "old_str": "xyz", "new_str": "abc"}));
        assert!(result.contains("not found"), "got: {result}");
    }

    #[test]
    fn list_dir_returns_entries() {
        let dir = tempfile::tempdir().unwrap();
        let executor = ToolExecutor::new(dir.path());
        std::fs::write(dir.path().join("a.txt"), "").unwrap();
        std::fs::create_dir(dir.path().join("subdir")).unwrap();

        let result = executor.list_dir(&json!({"path": "."}));
        assert!(result.contains("a.txt"), "got: {result}");
        assert!(result.contains("subdir/"), "got: {result}");
    }

    #[test]
    fn list_dir_skips_hidden() {
        let dir = tempfile::tempdir().unwrap();
        let executor = ToolExecutor::new(dir.path());
        std::fs::write(dir.path().join(".hidden"), "").unwrap();
        std::fs::write(dir.path().join("visible.txt"), "").unwrap();

        let result = executor.list_dir(&json!({"path": "."}));
        assert!(!result.contains(".hidden"), "should skip hidden: {result}");
        assert!(result.contains("visible.txt"));
    }

    // ── read_file with line ranges ────────────────────────────────────────────

    #[test]
    fn read_file_with_line_range() {
        let dir = tempfile::tempdir().unwrap();
        let executor = ToolExecutor::new(dir.path());
        executor.write_file(&json!({"path": "lines.txt", "content": "line1\nline2\nline3\nline4"}));

        let result =
            executor.read_file(&json!({"path": "lines.txt", "start_line": 2, "end_line": 3}));
        assert_eq!(result, "line2\nline3");
    }
}
