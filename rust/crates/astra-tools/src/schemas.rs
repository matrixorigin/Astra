//! Tool schema definitions for all edge tools.
//
//! Each schema is a JSON object following the OpenAI function-calling format:
//! `{ "type": "function", "function": { "name": ..., "description": ..., "parameters": ... } }`

use serde_json::{Value, json};

pub const DEFAULT_EXECUTOR_TOOL_NAMES: &[&str] = &[
    "bash",
    "read_file",
    "write_file",
    "str_replace",
    "list_dir",
    "grep",
    "glob",
    "git",
    "web_fetch",
    "web_search",
    "run_script",
    "notify",
    "ask_user",
    "task",
];

pub const SERVER_EXECUTOR_TOOL_NAMES: &[&str] = &[
    "bash",
    "read_file",
    "write_file",
    "str_replace",
    "list_dir",
    "grep",
    "glob",
    "git",
    "github",
    "memory",
    "session",
    "mo",
    "agent",
    "introspect",
    "lsp",
    "web_fetch",
    "web_search",
    "symbols",
    "run_script",
    "notify",
    "ask_user",
    "task",
];

fn filter_tool_schemas_by_name(allowed_names: &[&str]) -> Vec<Value> {
    all_tool_schemas()
        .into_iter()
        .filter(|schema| {
            schema
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
                .is_some_and(|name| allowed_names.contains(&name))
        })
        .collect()
}

pub fn default_executor_tool_schemas() -> Vec<Value> {
    filter_tool_schemas_by_name(DEFAULT_EXECUTOR_TOOL_NAMES)
}

pub fn server_executor_tool_schemas() -> Vec<Value> {
    filter_tool_schemas_by_name(SERVER_EXECUTOR_TOOL_NAMES)
}

pub fn all_tool_schemas() -> Vec<Value> {
    all_tool_schemas_with_env(|k| std::env::var(k).ok())
}

/// Like `all_tool_schemas()` but reads env via a caller-supplied closure.
/// The `env` parameter is currently unused (all gated tools have been
/// removed) but kept for forward compatibility with future per-env
/// opt-in surfaces.
pub fn all_tool_schemas_with_env<F: Fn(&str) -> Option<String>>(env: F) -> Vec<Value> {
    let _ = env; // reserved for future env-gated tools
    let mut schemas = all_tool_schemas_core();
    // run_script is Unix-only (UDS RPC transport). Always exposed on Unix —
    // no env gate, this is the production tool.
    #[cfg(unix)]
    {
        schemas.push(run_script_schema_default());
    }
    schemas.push(json!({
        "type": "function",
        "function": {
            "name": "powershell",
            "description": "Execute a PowerShell command. Use for Windows shell tasks, pwsh scripts, and cross-platform automation when PowerShell syntax is preferred over bash.",
            "parameters": {
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "PowerShell command to run"},
                    "timeout": {"type": "number", "description": "Timeout in seconds (default 120). Pass a larger value for long-running builds/tests (e.g. 300 for cargo build, 600 for full test suites)."}
                },
                "required": ["command"]
            }
        }
    }));
    schemas
}

/// Default `run_script` schema exposed when the caller has not yet wired
/// session-specific priority/enabled-tool hints. Uses the full sandbox
/// allowlist + Project mode + neutral priority. Sites that know the session
/// context (manifest_loader, repl_turn) should call
/// `run_script::build_run_script_schema` directly for a tighter schema.
#[cfg(unix)]
fn run_script_schema_default() -> Value {
    use std::collections::HashSet;
    let enabled: HashSet<String> = crate::run_script::RPC_ALLOWED_TOOLS
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    crate::run_script::build_run_script_schema(
        &enabled,
        crate::run_script::ExecutionMode::Project,
        crate::run_script::PriorityHint::Neutral,
    )
}

fn all_tool_schemas_core() -> Vec<Value> {
    vec![
        json!({
            "type": "function",
            "function": {
                "name": "bash",
                "description": "Execute a shell command in the project root. For builds, tests, installs, and CLI tasks. Use dedicated tools for: file reading (read_file), search (grep), file finding (glob), editing (str_replace), git operations (git).",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "command": {"type": "string", "description": "Shell command to run"},
                        "timeout": {"type": "number", "description": "Timeout in seconds (default 120). Pass a larger value for long-running builds/tests (e.g. 300 for cargo build, 600 for full test suites)."},
                        "force": {"type": "boolean", "description": "If true, bypass the per-session identical-command cache and execute even when the same command already succeeded."}
                    },
                    "required": ["command"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "read_file",
                "description": "Read file contents. Use start_line/end_line for large files. Set outline=true for function/class signatures only.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "File path relative to project root"},
                        "start_line": {"type": "integer", "minimum": 1, "description": "First line to read (1-based)"},
                        "end_line": {"type": "integer", "minimum": 1, "description": "Last line to read (inclusive)"},
                        "outline": {"type": "boolean", "description": "Return only function/class/struct signatures with line numbers"}
                    },
                    "required": ["path"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "write_file",
                "description": "Create or overwrite a file. Set delete=true to delete instead.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "File path relative to project root"},
                        "content": {"type": "string", "description": "File content (not required when delete=true)"},
                        "delete": {"type": "boolean", "description": "If true, delete the file instead of writing"}
                    },
                    "required": ["path"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "str_replace",
                "description": "Replace text in a file. Exact match with fuzzy fallback. Use edits array for batch replacements.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "File path relative to project root"},
                        "old_str": {"type": "string", "description": "Exact string to replace (single-edit mode)"},
                        "new_str": {"type": "string", "description": "Replacement string (single-edit mode)"},
                        "edits": {
                            "type": "array",
                            "description": "Array of {old_str, new_str} pairs applied atomically. Mutually exclusive with old_str/new_str.",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "old_str": {"type": "string"},
                                    "new_str": {"type": "string"}
                                },
                                "required": ["old_str", "new_str"]
                            }
                        },
                        "dry_run": {"type": "boolean", "description": "Preview diff without applying (default: false)"},
                        "replace_all": {"type": "boolean", "description": "Replace ALL occurrences (default: false)"},
                        "allow_structural_change": {"type": "boolean", "description": "Bypass structural safety checks for intentional syntax-breaking or comment-removing edits (default: false)"}
                    },
                    "required": ["path"]
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
                "description": "Search for a regex pattern in files. Returns matching lines with file:line format. Respects .gitignore.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "pattern": {"type": "string", "description": "Regex pattern to search for"},
                        "path": {"type": "string", "description": "Directory or file to search (default: project root)"},
                        "include": {"type": "string", "description": "File glob filter e.g. '*.rs'"},
                        "case_sensitive": {"type": "boolean", "description": "Case sensitive (default false)"},
                        "fixed_strings": {"type": "boolean", "description": "Treat pattern as literal string (like grep -F)"},
                        "max_matches": {"type": "integer", "description": "Max matches per file"},
                        "output_mode": {"type": "string", "enum": ["content", "files_with_matches", "count"], "description": "Output: content (default), files_with_matches, or count"}
                    },
                    "required": ["pattern"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "glob",
                "description": "Find files matching a glob pattern. Respects .gitignore/.astraignore when present and supports pagination via offset/head_limit.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "pattern": {"type": "string", "description": "Glob pattern e.g. '**/*.rs'"},
                        "path": {"type": "string", "description": "Root directory (default: project root)"},
                        "sort_by": {"type": "string", "enum": ["mtime", "path"], "description": "Sort matching files by newest modified time first (default 'mtime') or alphabetically by path."},
                        "offset": {"type": "integer", "minimum": 0, "description": "Skip first N matching files (for pagination)"},
                        "head_limit": {"type": "integer", "minimum": 0, "description": "Max matching files to return after offset. Defaults to 100; set 0 for unlimited."}
                    },
                    "required": ["pattern"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "symbols",
                "description": "Extract code symbols (functions, classes, structs, methods) from a file using AST parsing (tree-sitter). Supports Rust, Python, TypeScript/JavaScript, Go, Java, C/C++, Ruby. Returns structured symbol info with signatures, line numbers, and nesting. Use for: understanding file structure, finding specific symbols by name, generating documentation outlines.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "File path relative to project root"},
                        "pattern": {"type": "string", "description": "Optional regex pattern to filter symbols by name (e.g., 'test_', 'parse.*')"},
                        "kinds": {"type": "array", "items": {"type": "string"}, "description": "Optional filter by symbol kinds: fn, method, class, struct, trait, interface, enum, type, const, var"},
                        "calls": {"type": "boolean", "description": "If true, show function calls within each symbol's body. Helps understand code flow without reading full source."}
                    },
                    "required": ["path"]
                }
            }
        }),
        // ── Git mutation tools ─────────────────────────────────────────────
        json!({
            "type": "function",
            "function": {
                "name": "web_fetch",
                "description": "Fetch a URL and return structured JSON with metadata, extracted content (Markdown by default), and navigation links. Handles HTML-to-Markdown conversion, link discovery, and content truncation automatically.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "url": {"type": "string", "description": "URL to fetch (http:// or https://)"},
                        "format": {"type": "string", "enum": ["markdown", "text"], "description": "Output format for extracted content (default: markdown)"},
                        "max_content": {"type": "integer", "description": "Max extracted content characters (default 80000)"},
                        "timeout": {"type": "integer", "description": "Timeout in seconds (default 30)"},
                        "max_links": {"type": "integer", "description": "Max navigation links to extract (default 25)"}
                    },
                    "required": ["url"]
                }
            }
        }),
        // ─── Web search tool ──────────────────────────────────────────────────────
        json!({
            "type": "function",
            "function": {
                "name": "web_search",
                "description": "Perform a web search and get results. Returns search URLs for multiple engines that can be fetched with web_fetch. Use for finding current information, documentation, or answers not in local knowledge.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "The search query. Be specific for better results."
                        },
                        "engine": {
                            "type": "string",
                            "enum": ["google", "duckduckgo", "bing", "wikipedia", "github"],
                            "description": "Search engine to use (default: google). Use 'wikipedia' for encyclopedic info, 'github' for code/repos."
                        },
                        "num_results": {
                            "type": "integer",
                            "description": "Number of results to request (default: 10, max: 50)",
                            "default": 10
                        }
                    },
                    "required": ["query"]
                }
            }
        }),
        // ── send_message: Inter-agent messaging ────────────────────────────────
        json!({
            "type": "function",
            "function": {
                "name": "lsp",
                "description": "Language Server Protocol operations. Set dry_run=false to apply writes (rename, format, code_action).",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "operation": {
                            "type": "string",
                            "enum": [
                                "goto_definition","find_references","hover","document_symbols",
                                "workspace_symbols","call_hierarchy","incoming_calls","outgoing_calls",
                                "declaration","type_definition","implementation","supertypes","subtypes",
                                "prepare_rename","rename","code_actions","completions","signature_help",
                                "document_highlight","document_links","inlay_hints","folding_ranges",
                                "document_colors","color_presentations","semantic_tokens","code_lenses",
                                "selection_ranges","linked_editing_range",
                                "format_document","format_range","format_on_type","diagnostics"
                            ]
                        },
                        "file": {"type": "string", "description": "File path"},
                        "line": {"type": "integer", "description": "1-based line number"},
                        "column": {"type": "integer", "description": "1-based column"},
                        "end_line": {"type": "integer", "description": "End line (range ops)"},
                        "end_column": {"type": "integer", "description": "End column (range ops)"},
                        "symbol": {"type": "string", "description": "Symbol name (alternative to line/column)"},
                        "query": {"type": "string", "description": "Query (workspace_symbols)"},
                        "new_name": {"type": "string", "description": "New name (rename)"},
                        "dry_run": {"type": "boolean", "description": "Preview mode (default true)"},
                        "action_index": {"type": "integer", "minimum": 0, "description": "Code action index (default 0)"},
                        "item_index": {"type": "integer", "minimum": 0, "description": "Item index (completions/code_lenses)"},
                        "scope": {"type": "string", "enum": ["file", "project"]}
                    },
                    "required": ["operation"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "git",
                "description": "Git operations. Per-action required fields: commit→message, revert_commit→commit_sha, file_history→file, log_search→query, stash→sub_action, checkout_file→path+ref. Other actions have no extra required fields.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "action": {
                            "type": "string",
                            "enum": ["status","diff","log","show","blame","file_history","log_search","contributors","commit","revert_commit","stash","checkout_file","worktree"],
                            "description": "Git operation to perform"
                        },
                        "path": {
                            "type": "string",
                            "description": "Repository-relative file or directory path. Used by: diff, log, blame, checkout_file, contributors."
                        },
                        "file": {
                            "type": "string",
                            "description": "Repository-relative file path. Used by: file_history (required)."
                        },
                        "ref": {
                            "type": "string",
                            "description": "Git ref — commit SHA, branch, or tag. Used by: diff (compares ref vs worktree), log (restrict to ref), checkout_file (required: ref to restore from). Defaults to HEAD when omitted."
                        },
                        "base_ref": {
                            "type": "string",
                            "description": "Base ref for range diffs. Used by: diff (with ref: diff base_ref..ref)."
                        },
                        "revision": {
                            "type": "string",
                            "description": "Commit-ish to inspect. Used by: show. Defaults to HEAD."
                        },
                        "staged": {
                            "type": "boolean",
                            "description": "Show staged (index vs HEAD) changes. Used by: diff. Default false."
                        },
                        "n": {
                            "type": "integer",
                            "description": "Max entries to return. Used by: log (default 10, max 100), file_history (default 10), log_search (default 200)."
                        },
                        "query": {
                            "type": "string",
                            "description": "Search query (TF-IDF over commit messages). Used by: log_search (required)."
                        },
                        "since": {
                            "type": "string",
                            "description": "Git date expression (e.g. '2.weeks.ago', '2024-01-01'). Used by: contributors."
                        },
                        "message": {
                            "type": "string",
                            "description": "Commit message. Used by: commit (required), stash (optional, with sub_action=push/save)."
                        },
                        "all": {
                            "type": "boolean",
                            "description": "Stage all tracked modifications before committing. Used by: commit. Default false."
                        },
                        "commit_sha": {
                            "type": "string",
                            "description": "Commit SHA to revert. Used by: revert_commit (required)."
                        },
                        "sub_action": {
                            "type": "string",
                            "description": "Sub-operation for multi-mode actions. Used by: stash (push/save/pop/apply/drop/list), worktree (add/list/remove)."
                        },
                        "index": {
                            "type": "integer",
                            "description": "Stash index (stash@{N}). Used by: stash with sub_action=apply/pop/drop. Default 0."
                        },
                        "stash_ref": {
                            "type": "string",
                            "description": "Exact stash selector or OID. Used by: stash with sub_action=apply. Takes precedence over index."
                        }
                    },
                    "required": ["action"],
                    "x-astra-per-action-required": {
                        "commit": ["message"],
                        "revert_commit": ["commit_sha"],
                        "file_history": ["file"],
                        "log_search": ["query"],
                        "stash": ["sub_action"],
                        "checkout_file": ["path", "ref"],
                        "worktree": ["sub_action"]
                    }
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "github",
                "description": "GitHub operations. Per-action required fields: get_pr/ci_status→pr_number, get_issue→issue_number, create_issue→title. `repo` (owner/name or bare name) is inferred from git remote when omitted.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "action": {"type": "string", "enum": ["list_prs","get_pr","ci_status","repo_stats","list_issues","get_issue","create_issue"], "description": "GitHub operation"},
                        "repo": {"type": "string", "description": "owner/name or bare name (e.g. 'anthropics/claude-code' or 'memoria'). Inferred from current git remote when omitted."},
                        "pr_number": {"type": "integer", "description": "PR number. REQUIRED when action=get_pr or action=ci_status."},
                        "issue_number": {"type": "integer", "description": "Issue number. REQUIRED when action=get_issue."},
                        "title": {"type": "string", "description": "Issue title. REQUIRED when action=create_issue."},
                        "body": {"type": "string", "description": "Issue body (create_issue)."},
                        "labels": {"type": "array", "items": {"type": "string"}, "description": "Issue labels (create_issue)."},
                        "state": {"type": "string", "enum": ["open","closed","all"], "description": "PR/issue state filter (list_prs, list_issues)."},
                        "detail": {"type": "string", "enum": ["minimal","normal","full"], "description": "Response verbosity. Default normal."}
                    },
                    "required": ["action"],
                    "x-astra-per-action-required": {
                        "get_pr": ["pr_number"],
                        "ci_status": ["pr_number"],
                        "get_issue": ["issue_number"],
                        "create_issue": ["title"]
                    }
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "memory",
                "description": "Memory operations. Per-action required fields: store→content; retrieve/search→query; correct→new_content+reason (plus `memory_id` OR `query` to target); feedback→memory_id+signal. `purge` needs either `memory_id` or `topic` (not schema-enforced — document level).",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "action": {"type": "string", "enum": ["store","retrieve","purge","correct","profile","search","feedback"], "description": "Memory operation"},
                        "content": {"type": "string", "description": "Content. REQUIRED when action=store."},
                        "query": {"type": "string", "description": "Semantic query. REQUIRED when action=retrieve or action=search. Also used by correct to select target without a memory_id."},
                        "memory_id": {"type": "string", "description": "Memory identifier. REQUIRED when action=feedback. Used by purge/correct to target a specific memory."},
                        "new_content": {"type": "string", "description": "Replacement content. REQUIRED when action=correct."},
                        "reason": {"type": "string", "description": "Human-readable reason. REQUIRED when action=correct."},
                        "signal": {"type": "string", "enum": ["useful","irrelevant","outdated","wrong"], "description": "Feedback signal. REQUIRED when action=feedback."},
                        "topic": {"type": "string", "description": "Keyword for bulk purge. Provide `memory_id` OR `topic` when action=purge."},
                        "memory_type": {"type": "string", "enum": ["semantic","profile","procedural","working","episodic","tool_result"], "description": "Type: semantic (default), profile, procedural, working, episodic, tool_result"},
                        "session_id": {"type": "string", "description": "Optional session-scoped retrieval boost or store tag."},
                        "top_k": {"type": "integer", "description": "Retrieve/search result cap (default 5 for retrieve, 10 for search)."},
                        "agent_type": {"type": "string", "description": "Scope to a specific agent type (explore, code-review, task, general-purpose). When set on store, tags the memory; on retrieve/search, filters to only that type's memories + unscoped globals."}
                    },
                    "required": ["action"],
                    "x-astra-per-action-required": {
                        "store": ["content"],
                        "retrieve": ["query"],
                        "search": ["query"],
                        "correct": ["new_content", "reason"],
                        "feedback": ["memory_id", "signal"]
                    }
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "session",
                "description": "Session lifecycle and introspection. Per-action required fields: config→path+value; prioritize/deprioritize→tool; ask_user→question; tool_search→query. Other actions (compact, rollback_edits, sleep, timeline, summary, history) have no extra required fields.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "action": {"type": "string", "enum": ["config","prioritize","deprioritize","set_goal","compact","rollback_edits","ask_user","sleep","tool_search","timeline","summary","history"]},
                        "path": {"type": "string", "description": "Dot-separated config path (e.g. 'compression.compression_threshold'). REQUIRED when action=config. Also used as file path when action=rollback_edits and scope=file."},
                        "value": {"description": "Config value (any JSON scalar matching the path's type). REQUIRED when action=config."},
                        "force": {"type": "boolean", "description": "Override per-turn mutation ceiling / drift cap (config)."},
                        "tool": {"type": "string", "description": "Tool name. REQUIRED when action=prioritize or action=deprioritize."},
                        "goal": {"type": "string", "description": "Goal text (set_goal)."},
                        "scope": {"type": "string", "enum": ["current_turn","turn","file","list"], "description": "Rollback scope (rollback_edits)."},
                        "turn_index": {"type": "integer", "description": "Turn index (rollback_edits scope=turn)."},
                        "question": {"type": "string", "description": "Question to ask. REQUIRED when action=ask_user."},
                        "choices": {"type": "array", "items": {"type": "string"}, "description": "2-9 options for multiple choice (ask_user)."},
                        "default": {"type": "string", "description": "Default answer used if user presses Enter (ask_user)."},
                        "context": {"type": "string", "description": "Brief context shown above the question (ask_user)."},
                        "duration_ms": {"type": "integer", "description": "Sleep ms, max 300000 (sleep)."},
                        "reason": {"type": "string", "description": "Reason for sleep."},
                        "query": {"type": "string", "description": "Query string. REQUIRED when action=tool_search."},
                        "max_results": {"type": "integer", "description": "Max results (tool_search, default 5)."}
                    },
                    "required": ["action"],
                    "x-astra-per-action-required": {
                        "config": ["path", "value"],
                        "prioritize": ["tool"],
                        "deprioritize": ["tool"],
                        "ask_user": ["question"],
                        "tool_search": ["query"]
                    }
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "mo",
                "description": "MatrixOne database operations. Per-action required fields: query→sql; snapshot/branch→sub_action (create/list/drop/restore). For sub_action=create/drop/restore on snapshot, `name` is required.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "action": {"type": "string", "enum": ["query","snapshot","branch"], "description": "MO operation"},
                        "sql": {"type": "string", "description": "SQL to execute. REQUIRED when action=query."},
                        "sub_action": {"type": "string", "enum": ["create","list","drop","delete","restore"], "description": "Snapshot/branch sub-operation. REQUIRED when action=snapshot or action=branch."},
                        "name": {"type": "string", "description": "Snapshot/branch name (alphanumeric/_/-, max 64 chars). Required for snapshot create/drop/restore; branch create auto-derives from git branch when omitted."},
                        "database": {"type": "string", "description": "Target database name (snapshot scope). Defaults to current."}
                    },
                    "required": ["action"],
                    "x-astra-per-action-required": {
                        "query": ["sql"],
                        "snapshot": ["sub_action"],
                        "branch": ["sub_action"]
                    }
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "agent",
                "description": "Multi-agent operations. Per-action required fields: spawn requires `description`+`prompt`; delegate requires `task`; run_chain requires `steps`; get_result requires `agent_id`; send_message requires `to`+`message`.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "action": {"type": "string", "enum": ["delegate","run_chain","spawn","get_result","send_message"]},
                        "task": {"type": "string", "description": "Task description. REQUIRED when action=delegate."},
                        "steps": {"type": "array", "description": "Chain steps. REQUIRED when action=run_chain."},
                        "description": {"type": "string", "description": "Short (3-5 word) task description. REQUIRED when action=spawn."},
                        "prompt": {"type": "string", "description": "Detailed task prompt. REQUIRED when action=spawn."},
                        "agent_type": {"type": "string", "enum": ["explore","code-review","task","general-purpose"], "description": "Agent type (spawn). Default: general-purpose."},
                        "model": {"type": "string", "description": "Model override (spawn)."},
                        "background": {"type": "boolean", "description": "Return immediately with agent_id; you MUST call action=get_result later (spawn)."},
                        "name": {"type": "string", "description": "Addressable name for send_message routing (spawn)."},
                        "max_turns": {"type": "integer", "description": "Max turns (spawn). Explicit value wins over `complexity`."},
                        "complexity": {"type": "string", "enum": ["light","normal","deep"], "description": "Task-complexity hint scaling the default budget when `max_turns` is absent. `light`≈10 turns, `normal`=agent default, `deep`=2× default. Use `deep` for review/refactor/multi-file tasks that routinely exhaust the default."},
                        "isolated": {"type": "boolean", "description": "Use isolated worktree (spawn)."},
                        "allowed_tools": {"type": "array", "items": {"type": "string"}, "description": "Tool allowlist override (spawn)."},
                        "agent_id": {"type": "string", "description": "Agent ID. REQUIRED when action=get_result."},
                        "to": {"type": "string", "description": "Recipient agent_id or '*'. REQUIRED when action=send_message."},
                        "message": {"description": "Message content. REQUIRED when action=send_message."},
                        "message_type": {"type": "string", "enum": ["text","question","answer","instruction","progress","result","shutdown_request","shutdown_response"]},
                        "priority": {"type": "string", "enum": ["low","normal","high"]}
                    },
                    "required": ["action"],
                    "x-astra-per-action-required": {
                        "spawn": ["description", "prompt"],
                        "delegate": ["task"],
                        "run_chain": ["steps"],
                        "get_result": ["agent_id"],
                        "send_message": ["to", "message"]
                    }
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "introspect",
                "description": "Query own runtime state. Subtopics: 'session' (default — token pressure, cache hit rate, tool health, alerts, working memory); 'cache' (cache-regression diagnosis over recent LLM captures); 'recent' (last N LLM-round summaries from in-memory ring — tokens, tool calls, duration); 'volatile' (what runtime nudges / working-set / coaching are about to be injected); 'stall' (loop-guard state — nudge count, stall events, forced corrections); 'all' (session + recent + volatile + stall).",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "subtopic": {"type": "string", "enum": ["session","cache","recent","volatile","stall","noise","all"], "description": "Which diagnostic to run (default: session). `noise`: per-channel freshness of runtime-injected prompt signals — flags channels re-rendered unchanged for many turns."},
                        "detail": {"type": "string", "enum": ["full","summary","minimal"], "description": "Output detail level for the session topic (default: auto from budget). Ignored for other subtopics."}
                    }
                }
            }
        }),
        // ── Notify (proactive notification for gateways) ─────────────────
        json!({
            "type": "function",
            "function": {
                "name": "notify",
                "description": "Send a notification to the user. Use for proactive updates (background task done, blocker found, unsolicited insight). Gateways route based on notification_type: 'normal' = in-chat reply, 'proactive' = push notification. CLI mode: both render as text.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "message": {"type": "string", "description": "Notification content"},
                        "notification_type": {"type": "string", "enum": ["normal","proactive"], "description": "Routing hint for gateway. 'proactive' = push even if user isn't looking at chat."}
                    },
                    "required": ["message"]
                }
            }
        }),
        // ── Ask user (interactive clarification) ─────────────────────────
        json!({
            "type": "function",
            "function": {
                "name": "ask_user",
                "description": "Ask the user a question when you need clarification or a decision. Supports multiple-choice (2-9 options, single-key select) and free-form text input. Use sparingly — only when the next step is genuinely ambiguous.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "question": {"type": "string", "description": "The question to ask"},
                        "choices": {"type": "array", "items": {"type": "string"}, "description": "2-9 options for multiple choice. Omit for free-form input."},
                        "default": {"type": "string", "description": "Default answer (used if user presses Enter without typing)"},
                        "context": {"type": "string", "description": "Brief context shown above the question (dimmed)"}
                    },
                    "required": ["question"]
                }
            }
        }),
        // ── Task management (unified tool) ───────────────────────────────
        json!({
            "type": "function",
            "function": {
                "name": "task",
                "description": "Track work progress for multi-step tasks. Per-action required fields: create→title; update/get/stop→task_id.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "action": {"type": "string", "enum": ["create","update","list","get","stop"], "description": "Operation to perform"},
                        "title": {"type": "string", "description": "Brief imperative title. REQUIRED when action=create."},
                        "description": {"type": "string", "description": "(create/update) What needs to be done"},
                        "task_id": {"type": "string", "description": "Task ID (e.g. 'task-1'). REQUIRED when action=update, get, or stop."},
                        "new_status": {"type": "string", "enum": ["pending","in_progress","completed","failed","deleted"], "description": "(update) New status to assign. 'deleted' permanently removes the task."},
                        "status_filter": {"type": "string", "enum": ["pending","in_progress","completed","failed","all","active"], "description": "(list) Restrict results. 'active' = pending+in_progress. Default 'all'."},
                        "subtask_id": {"type": "string", "description": "(update) Update a specific subtask"},
                        "active_form": {"type": "string", "description": "(create/update) Present-continuous form shown while in_progress (e.g. 'Running tests')"},
                        "owner": {"type": "string", "description": "(create/update) Agent or user that owns this task"},
                        "metadata": {"type": "object", "description": "(create/update) Arbitrary key-value pairs. On update: set key to null to delete it."},
                        "add_blocks": {"type": "array", "items": {"type": "string"}, "description": "(update) Task IDs that THIS task blocks (they can't start until this completes)"},
                        "add_blocked_by": {"type": "array", "items": {"type": "string"}, "description": "(update) Task IDs that must complete before THIS task can start"},
                        "remove_blocks": {"type": "array", "items": {"type": "string"}, "description": "(update) Remove entries from this task's blocks list"},
                        "remove_blocked_by": {"type": "array", "items": {"type": "string"}, "description": "(update) Remove entries from this task's blocked_by list"},
                        "subtasks": {
                            "type": "array",
                            "description": "(create) Optional sub-steps",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "id": {"type": "string"},
                                    "title": {"type": "string"},
                                    "description": {"type": "string"},
                                    "depends_on": {"type": "array", "items": {"type": "string"}}
                                },
                                "required": ["id", "title"]
                            }
                        },
                        "reason": {"type": "string", "description": "(stop) Why the task is being stopped"},
                        "error_message": {"type": "string", "description": "(update) Reason for failure"}
                    },
                    "required": ["action"],
                    "x-astra-per-action-required": {
                        "create": ["title"],
                        "update": ["task_id"],
                        "get": ["task_id"],
                        "stop": ["task_id"]
                    }
                }
            }
        }),
    ]
}

#[cfg(test)]
#[allow(dead_code, unused_imports, clippy::empty_line_after_doc_comments)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    fn schema_names(schemas: &[Value]) -> Vec<&str> {
        schemas
            .iter()
            .filter_map(|schema| {
                schema
                    .get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(Value::as_str)
            })
            .collect()
    }

    // execute_code has been deleted. The only hallucination-prevention
    // concern now is ensuring run_script is advertised on Unix, and that
    // `execute_code` is NOT in the schema list (so the model doesn't
    // hallucinate it).

    #[test]
    fn execute_code_no_longer_present_in_schemas() {
        let schemas = all_tool_schemas_with_env(|_| None);
        let names = schema_names(&schemas);
        assert!(
            !names.contains(&"execute_code"),
            "execute_code was removed; legacy references must not leak into the schema list"
        );
    }

    // ── run_script schema visibility ──────────────────────────────────────

    #[cfg(unix)]
    #[test]
    fn run_script_visible_by_default_on_unix() {
        // run_script is the production successor to execute_code and must
        // be discoverable without any opt-in env var.
        let schemas = all_tool_schemas_with_env(|_| None);
        let names = schema_names(&schemas);
        assert!(
            names.contains(&"run_script"),
            "run_script must appear in the default schema list so the LLM can discover it"
        );
    }

    #[cfg(unix)]
    #[test]
    fn run_script_default_schema_lists_all_sandbox_tools() {
        let schemas = all_tool_schemas_with_env(|_| None);
        let rs = schemas
            .iter()
            .find(|s| {
                s.get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(Value::as_str)
                    == Some("run_script")
            })
            .expect("run_script schema present");
        let desc = rs["function"]["description"].as_str().unwrap();
        // At least read_file and web_fetch should be mentioned — they're
        // the staple tools for multi-step pipelines.
        assert!(
            desc.contains("read_file"),
            "default schema should list read_file"
        );
        assert!(
            desc.contains("web_fetch"),
            "default schema should list web_fetch"
        );
    }

    #[cfg(not(unix))]
    #[test]
    fn run_script_hidden_on_non_unix() {
        let schemas = all_tool_schemas_with_env(|_| None);
        let names = schema_names(&schemas);
        assert!(
            !names.contains(&"run_script"),
            "run_script requires Unix domain sockets — must not appear on other platforms"
        );
    }

    #[test]
    fn default_executor_tool_schemas_match_supported_surface() {
        let schemas = default_executor_tool_schemas();
        let names = schema_names(&schemas);
        for &name in DEFAULT_EXECUTOR_TOOL_NAMES {
            assert!(
                names.contains(&name),
                "{name} should have a default executor schema"
            );
        }
        assert!(!names.contains(&"tool_search"));
        assert!(!names.contains(&"github_list_prs"));
        assert!(!names.contains(&"symbols"));
        assert!(!names.contains(&"rollback_file_edits"));
        assert!(!names.contains(&"memory_store"));
        assert!(!names.contains(&"powershell"));
        assert!(!names.contains(&"multi_edit"));
        // Legacy top-level tool names removed in dd556ec — these actions now
        // live under the consolidated `session` / `write_file` / `str_replace`
        // surfaces. Catch any accidental re-registration.
        assert!(!names.contains(&"delete_file"));
        assert!(!names.contains(&"enter_plan_mode"));
        assert!(!names.contains(&"exit_plan_mode"));
        // ask_user is now a first-class tool (P2 completion)
        assert!(names.contains(&"ask_user"));
        assert!(!names.contains(&"sleep"));
    }

    #[test]
    fn server_allowlist_each_name_has_function_schema() {
        let schemas = all_tool_schemas();
        let names = schema_names(&schemas);
        let set: HashSet<&str> = names.into_iter().collect();
        for &name in SERVER_EXECUTOR_TOOL_NAMES {
            assert!(
                set.contains(name),
                "SERVER_EXECUTOR_TOOL_NAMES contains `{name}` but all_tool_schemas() has no matching function.name"
            );
        }
    }

    /// Catches the opposite drift class: a `memory_*` schema exists but the tool was never added to the server allowlist.

    /// Default CLI/edge executor names must remain promotable to the server allowlist without
    /// forgetting to add new defaults when expanding server coverage.
    #[test]
    fn default_executor_tool_names_are_subset_of_server_allowlist() {
        let allow: HashSet<&str> = SERVER_EXECUTOR_TOOL_NAMES.iter().copied().collect();
        for name in DEFAULT_EXECUTOR_TOOL_NAMES {
            assert!(
                allow.contains(name),
                "`{name}` is in DEFAULT_EXECUTOR_TOOL_NAMES but missing from SERVER_EXECUTOR_TOOL_NAMES"
            );
        }
    }

    #[test]
    fn default_executor_tool_schemas_are_subset_of_server_executor_schemas() {
        let server_schemas = server_executor_tool_schemas();
        let server_names: HashSet<&str> = schema_names(&server_schemas).into_iter().collect();
        for name in schema_names(&default_executor_tool_schemas()) {
            assert!(
                server_names.contains(name),
                "default executor exposes `{name}` but server_executor_tool_schemas() omits it"
            );
        }
    }

    // ── Session 0e37eb46 regression: bash/powershell schema-advertised
    //    timeout defaults must match the actual code defaults ─────────
    //
    // The bash schema previously advertised "default 30" while the code
    // default is 120s. The LLM read "30" from the schema and either
    // (a) explicitly set timeout:30 and was surprised when cargo build
    // took 40s, or (b) expected cargo builds to finish within a 30s
    // mental deadline. Result: session 0e37eb46 r9 timed out at 30s,
    // model added `timeout: 180` at r11 — one round burned on a
    // schema-doc mismatch.
    //
    // The schemas must not lie. Lock: bash & powershell schemas must
    // document the ACTUAL default (120s), and the description must
    // hint that long-running commands (cargo, make, pytest, go test)
    // should pass a larger explicit timeout.

    fn find_schema<'a>(schemas: &'a [Value], name: &str) -> Option<&'a Value> {
        schemas.iter().find(|s| {
            s.get("function")
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
                == Some(name)
        })
    }

    fn timeout_description(schema: &Value) -> &str {
        schema
            .pointer("/function/parameters/properties/timeout/description")
            .and_then(Value::as_str)
            .unwrap_or("")
    }

    #[test]
    fn bash_schema_timeout_default_matches_code_default() {
        let schemas = all_tool_schemas_with_env(|_| None);
        let bash = find_schema(&schemas, "bash").expect("bash schema must exist");
        let desc = timeout_description(bash);
        assert!(
            desc.contains("120"),
            "bash schema must document the REAL default timeout (120s) — got {desc:?}. \
             Drift between schema and code burns a round per session when the LLM \
             hits unexpected timeout (session 0e37eb46 r9)."
        );
        assert!(
            !desc.contains("default 30"),
            "bash schema must NOT advertise default=30 when the code default is 120s"
        );
    }

    #[test]
    fn bash_schema_hints_to_extend_timeout_for_long_commands() {
        let schemas = all_tool_schemas_with_env(|_| None);
        let bash = find_schema(&schemas, "bash").expect("bash schema must exist");
        let desc = timeout_description(bash);
        // Presence of at least ONE of these signal tokens tells the
        // model "bump timeout for slow commands" without the schema
        // over-prescribing which tools are slow.
        let has_hint = ["cargo", "build", "test", "long"]
            .iter()
            .any(|kw| desc.to_lowercase().contains(kw));
        assert!(
            has_hint,
            "bash schema should hint that cargo/test/build-style commands \
             need a larger explicit timeout — got {desc:?}"
        );
    }

    #[test]
    fn powershell_schema_timeout_default_matches_code_default() {
        let schemas = all_tool_schemas_with_env(|_| None);
        let ps = find_schema(&schemas, "powershell").expect("powershell schema must exist");
        let desc = timeout_description(ps);
        assert!(
            desc.contains("120"),
            "powershell schema must document the REAL default (120s), got {desc:?}"
        );
    }
}
