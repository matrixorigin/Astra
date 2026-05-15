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
    "tool_search",
    "lsp",
    "web_fetch",
    "web_search",
    "publish_artifact",
    "symbols",
    "run_script",
    "notify",
    "ask_user",
    "task",
];

/// RPC tools exposed inside server-side `run_script`.
///
/// This is intentionally narrower than [`crate::run_script::RPC_ALLOWED_TOOLS`]:
/// the web/API server must only advertise sub-tools that the
/// `ServerToolExecutor` can actually route in-process. Keeping this list next
/// to `SERVER_EXECUTOR_TOOL_NAMES` makes capability drift visible during code
/// review and prevents the LLM from being told to call a Python-side helper
/// that will later fail at runtime.
pub const SERVER_RUN_SCRIPT_RPC_TOOL_NAMES: &[&str] = &[
    "read_file",
    "write_file",
    "list_dir",
    "grep",
    "web_fetch",
    "bash",
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
    let mut schemas = filter_tool_schemas_by_name(SERVER_EXECUTOR_TOOL_NAMES);
    #[cfg(unix)]
    {
        if let Some(slot) = schemas.iter_mut().find(|schema| {
            schema
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
                == Some("run_script")
        }) {
            *slot = run_script_schema_for(SERVER_RUN_SCRIPT_RPC_TOOL_NAMES);
        }
    }
    schemas
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
    run_script_schema_for(crate::run_script::RPC_ALLOWED_TOOLS)
}

#[cfg(unix)]
fn run_script_schema_for(enabled_tool_names: &[&str]) -> Value {
    use std::collections::HashSet;
    let enabled: HashSet<String> = enabled_tool_names
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
                "name": "publish_artifact",
                "description": "Publish a file that was already generated in the current session workspace or /tmp so the web UI can preview and download it. Use this after creating images, PDFs, CSVs, Markdown, HTML, or other files with bash/write_file/run_script. Do not use this to generate content directly; first create the file, then publish its path.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Path of the generated file. Relative paths are resolved under the session workspace. Absolute paths are allowed only under the session workspace or /tmp."
                        },
                        "title": {"type": "string", "description": "Optional short display title for the artifact."},
                        "artifact_kind": {"type": "string", "description": "Optional stable kind such as image, pdf, markdown, html, data, text, code, archive, or file. If omitted, Astra infers it from the filename/content type."},
                        "content_type": {"type": "string", "description": "Optional MIME type. If omitted, Astra infers it from the file extension."},
                        "description": {"type": "string", "description": "Optional one-sentence explanation shown next to the artifact in the web UI."}
                    },
                    "required": ["path"]
                }
            }
        }),
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
                "description": "Git operations. Actions: status, diff, log, show, blame, file_history, log_search, contributors, commit, revert_commit, stash, checkout_file, worktree.",
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
                        "body": {"type": "string", "description": "Issue body (create_issue)."}
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
                "description": "Cognitive memory operations. Actions (model-facing verbs): \
                    `remember` (persist a fact for future turns/sessions), \
                    `recall` (retrieve relevant memories by query — combines keyword + vector + temporal + confidence), \
                    `expand` (drill into one memory by id to get overview/detail + linked related memories), \
                    `forget` (soft-delete by id), \
                    `update` (correct or enrich an existing memory), \
                    `focus` (session-scoped attention boost: subsequent `recall` weights the given topic/tag higher for ttl_secs), \
                    `reflect` (cross-memory pattern synthesis; consolidates recent memories into higher-level scenes), \
                    `profile` (return the user profile summary), \
                    `feedback` (signal that a recalled memory was useful/irrelevant/outdated/wrong — shapes future recall ranking). \
                    Supports `agent_type` scoping for per-agent-type memory isolation. \
                    Visibility: `visibility=\"private\"` (default) keeps the memory to your account; \
                    `visibility=\"team\"` tags it for team sharing (requires `team_id`), and on recall \
                    the union includes the current user's team-tagged memories.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "action": {
                            "type": "string",
                            "enum": ["remember","recall","expand","forget","update","focus","reflect","profile","feedback"],
                            "description": "Cognitive memory verb."
                        },
                        "content": {"type": "string", "description": "remember: the fact to store. update: replacement content."},
                        "query": {"type": "string", "description": "recall: the natural-language query. update: also accepted as a selector when memory_id is unknown."},
                        "memory_id": {"type": "string", "description": "expand / forget / update / feedback: target memory id."},
                        "memory_type": {
                            "type": "string",
                            "enum": ["semantic","profile","procedural","working","episodic"],
                            "description": "remember: memory category. Defaults to `semantic`."
                        },
                        "top_k": {"type": "integer", "description": "recall: max number of memories (default 10)."},
                        "min_confidence": {"type": "number", "description": "recall: filter out memories below this confidence (0.0-1.0)."},
                        "scope": {
                            "type": "string",
                            "enum": ["all","session"],
                            "description": "recall: `session` restricts to current session; `all` includes cross-session (default `all`)."
                        },
                        "view": {
                            "type": "string",
                            "enum": ["compact","overview","full"],
                            "description": "recall: response detail level. `compact` (default) returns abstracts; `overview` adds summaries; `full` adds linked memories."
                        },
                        "importance": {"type": "number", "description": "remember / update: importance score (0.0-1.0)."},
                        "trust_tier": {"type": "string", "description": "remember / update: provenance trust tier."},
                        "tags": {"type": "array", "items": {"type": "string"}, "description": "remember: tags to attach."},
                        "tags_add": {"type": "array", "items": {"type": "string"}, "description": "update: tags to append."},
                        "tags_remove": {"type": "array", "items": {"type": "string"}, "description": "update: tags to detach."},
                        "visibility": {
                            "type": "string",
                            "enum": ["private","team"],
                            "description": "remember: who else should see this memory. `private` (default) — only you on this account. `team` — shared with other agents that belong to the same team (`team_id` required). recall: when set to `team`, the retrieval union includes team-tagged memories in addition to your private ones."
                        },
                        "team_id": {
                            "type": "string",
                            "description": "remember with visibility=team: the team to tag (encoded as `astra:team:<id>` in the memory's tag set). recall with visibility=team: union with the given team's shared pool. Omit to fall back to the executor's default team from session context."
                        },
                        "reason": {"type": "string", "description": "forget / update: REQUIRED — non-empty explanation for the audit trail (why this memory is being changed). feedback: optional."},
                        "level": {
                            "type": "string",
                            "enum": ["abstract","overview","detail","linked"],
                            "description": "expand: how deep to unfold (`abstract` < `overview` < `detail` < `linked`)."
                        },
                        "focus_type": {
                            "type": "string",
                            "enum": ["topic","tag","memory_id","session"],
                            "description": "focus: what kind of attention to boost."
                        },
                        "focus_value": {"type": "string", "description": "focus: the topic / tag / id / session to boost."},
                        "boost": {"type": "number", "description": "focus: multiplier applied to matching memories (default 1.5)."},
                        "ttl_secs": {"type": "integer", "description": "focus: how long the boost lasts (default 3600s)."},
                        "signal": {
                            "type": "string",
                            "enum": ["useful","irrelevant","outdated","wrong"],
                            "description": "feedback: quality signal for the recalled memory."
                        },
                        "context": {"type": "string", "description": "feedback: optional free-form context on why the signal was given."},
                        "agent_type": {
                            "type": "string",
                            "enum": ["explore","code-review","task","general-purpose"],
                            "description": "remember / recall: scope to a specific agent type. On remember it tags; on recall it filters to that type + unscoped globals."
                        }
                    },
                    "required": ["action"],
                    "x-astra-per-action-required": {
                        "remember": ["content"],
                        "recall": ["query"],
                        "expand": ["memory_id"],
                        "forget": ["reason"],
                        "update": ["reason"],
                        "feedback": ["memory_id", "signal"]
                    }
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "session",
                "description": "Session lifecycle and introspection. Actions: config, prioritize, deprioritize, set_goal, compact, enter_plan, exit_plan, rollback_edits, ask_user, sleep, timeline, summary, history_page, history_search, history_around. Use the history_* actions when the user refers to older turns in this same chat and the visible context is insufficient.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "action": {"type": "string", "enum": ["config","prioritize","deprioritize","set_goal","compact","enter_plan","exit_plan","rollback_edits","ask_user","sleep","timeline","summary","history_page","history_search","history_around"]},
                        "key": {"type": "string", "description": "Config key"},
                        "value": {"type": "string", "description": "Config value"},
                        "tool": {"type": "string", "description": "Tool name (prioritize/deprioritize)"},
                        "goal": {"type": "string", "description": "Goal text"},
                        "scope": {"type": "string", "enum": ["current_turn","turn","file","list"], "description": "Rollback scope"},
                        "path": {"type": "string", "description": "File path (rollback scope=file)"},
                        "turn_index": {"type": "integer", "description": "Turn index (rollback scope=turn)"},
                        "question": {"type": "string", "description": "Question (ask_user)"},
                        "choices": {"type": "array", "items": {"type": "string"}, "description": "Choices 2-9 (ask_user)"},
                        "default": {"type": "string", "description": "Default answer (ask_user)"},
                        "context": {"type": "string", "description": "Brief context (ask_user)"},
                        "duration_ms": {"type": "integer", "description": "Sleep ms, max 300000"},
                        "reason": {"type": "string", "description": "Reason (sleep)"},
                        "pattern": {"type": "string", "description": "history_search search text: compact topic, phrase, filename, error text, decision, or Chinese/English keyword."},
                        "before_seq": {"type": "integer", "description": "history_page/history_search cursor: return transcript rows older than this item_seq."},
                        "after_seq": {"type": "integer", "description": "history_page/history_search cursor: return transcript rows newer than this item_seq."},
                        "item_seq": {"type": "integer", "description": "history_around anchor returned by history_page/history_search."},
                        "radius": {"type": "integer", "description": "history_around rows before and after item_seq, 0-10, default 3."},
                        "limit": {"type": "integer", "description": "history_page/history_search row/result limit. history_page: 1-50 default 20; history_search: 1-20 default 8."},
                        "scan_limit": {"type": "integer", "description": "history_search recent transcript scan limit, 50-1000, default 400."},
                        "order": {"type": "string", "enum": ["asc","desc"], "description": "history_page output order. asc reads a recovered range chronologically; desc browses backward from newest."},
                        "role": {"type": "string", "enum": ["all","user","assistant","system"], "description": "Optional history role filter. Default all."}
                    },
                    "required": ["action"],
                    "x-astra-per-action-required": {
                        "config": ["path", "value"],
                        "prioritize": ["tool"],
                        "deprioritize": ["tool"],
                        "ask_user": ["question"],
                        "history_search": ["pattern"]
                    }
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "mo",
                "description": "MatrixOne database operations. Actions: query, snapshot, branch.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "action": {"type": "string", "enum": ["query","snapshot","branch"], "description": "MO operation"},
                        "sql": {"type": "string", "description": "SQL to execute (for query)"},
                        "name": {"type": "string", "description": "Snapshot/branch name"}
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
                "description": "Multi-agent operations. Actions: spawn, get_result, run_chain, send_message.\n\n\
        ## Execution mode\n\
        - **Default (synchronous)**: `spawn` blocks until the sub-agent's final result is ready. Use this for work you depend on in the current turn. The sub-agent's tool calls stream back inline — the TUI renders them inside the parent Task card so the user sees progress live.\n\
        - **Background**: pass `run_in_background: true` (alias: legacy `background: true`) to return immediately with `{agent_id}`. Use this for fire-and-forget or long-running work you don't need to await; follow up with `get_result` later. Durable-task store persists the run across session death so it survives `astra` restarts.\n\n\
        ## Parallel sub-agent fan-out\n\
        To run N sub-agents in parallel (e.g. multi-angle code review), emit N `agent` tool calls **in a single assistant message**, each with `action='spawn'` and `run_in_background: true`. They run concurrently. After all are spawned, call `agent(action='get_result', agent_id=...)` for each one — `get_result` blocks until that child finishes. This is the ONLY way to fan out parallel agents; do not use `action='delegate'` (removed: it had no execution backend).",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "action": {"type": "string", "enum": ["spawn","get_result","run_chain","send_message"]},
                        "steps": {"type": "array", "description": "Chain steps (run_chain)"},
                        "description": {"type": "string", "description": "Short task description (spawn)"},
                        "prompt": {"type": "string", "description": "Detailed prompt (spawn)"},
                        "agent_type": {"type": "string", "enum": ["explore","code-review","task","general-purpose"]},
                        "model": {"type": "string", "description": "Model override (spawn)"},
                        "run_in_background": {"type": "boolean", "description": "If true, return immediately with agent_id instead of blocking on the sub-agent's final result. Default false (sync). Applies to spawn."},
                        "background": {"type": "boolean", "description": "(Deprecated alias for run_in_background.) If true, return immediately with agent_id (spawn)."},
                        "name": {"type": "string", "description": "Addressable name (spawn)"},
                        "max_turns": {"type": "integer", "description": "Max turns (spawn). Explicit value wins over `complexity`."},
                        "complexity": {"type": "string", "enum": ["light","normal","deep"], "description": "Task-complexity hint scaling the default budget when `max_turns` is absent. `light`≈10 turns, `normal`=agent default, `deep`=2× default. Use `deep` for review/refactor/multi-file tasks that routinely exhaust the default."},
                        "isolated": {"type": "boolean", "description": "Use isolated worktree (spawn)"},
                        "allowed_tools": {"type": "array", "items": {"type": "string"}, "description": "Tool allowlist (spawn)"},
                        "agent_id": {"type": "string", "description": "Agent ID (get_result)"},
                        "to": {"type": "string", "description": "Recipient agent_id or '*' (send_message)"},
                        "message": {"description": "Message content (send_message)"},
                        "message_type": {"type": "string", "enum": ["text","question","answer","instruction","progress","result","shutdown_request","shutdown_response"]},
                        "priority": {"type": "string", "enum": ["low","normal","high"]}
                    },
                    "required": ["action"],
                    "x-astra-per-action-required": {
                        "spawn": ["description", "prompt"],
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
        json!({
            "type": "function",
            "function": {
                "name": "tool_search",
                "description":
                    "Search and activate deferred tools. Pass `query` to find tools by \
                     keyword, OR pass `query=\"select:NAME\"` (or `select:NAME1,NAME2`) to \
                     retrieve the full schema for one or more deferred tools listed in \
                     `<deferred_tools>`. After calling with `select:`, you may invoke the \
                     selected tool(s) directly on the next turn — runtime accepts calls \
                     for any dispatchable name, not just names currently in `tools[]`.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description":
                                "Keyword query, or `select:NAME` / `select:NAME1,NAME2` for \
                                 direct activation."
                        },
                        "max_results": {
                            "type": "integer",
                            "description": "Maximum results for keyword mode (default 5, max 20)."
                        }
                    },
                    "required": ["query"]
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
        //
        // Note on description size (cache-aware):
        // This tool is pinned (always in the static tool-prefix), so its
        // description participates in the Anthropic cache breakpoint on
        // the last pinned tool. Expanding the description costs tokens
        // only on cache-miss turns (cache_write premium); on steady-state
        // cache-hit turns the full text is free. Empirically the hit-rate
        // runs 44–90% in interactive sessions, so a ~450-token expansion
        // amortises to ~100 effective tokens per turn — materially less
        // than the cost of the model re-deriving "when to use task" from
        // a 45-token hint and backtracking.
        json!({
            "type": "function",
            "function": {
                "name": "task",
                "description": "Use this tool to create and manage a structured task list for your current coding session. This helps you track progress, organize complex tasks, and demonstrate thoroughness to the user. It also helps the user understand the progress of the task and overall progress of their requests.\n\
        \n\
        Actions: create, update, list, get, stop, background_shell, background_agent, output, kill. Supports subtasks, blocking dependencies (add_blocks / add_blocked_by), ownership, and arbitrary metadata.\n\
        \n\
        ## When to Use This Tool\n\
        Use this tool proactively in these scenarios:\n\
        \n\
        1. Complex multi-step tasks - When a task requires 3 or more distinct steps or actions\n\
        2. Non-trivial and complex tasks - Tasks that require careful planning or multiple operations\n\
        3. User explicitly requests todo list - When the user directly asks you to use the todo list\n\
        4. User provides multiple tasks - When users provide a list of things to be done (numbered or comma-separated)\n\
        5. After receiving new instructions - Immediately capture user requirements as tasks\n\
        6. When you start working on a task - Mark it as `in_progress` BEFORE beginning work. Ideally you should only have ONE task as `in_progress` at a time\n\
        7. After completing a task - Mark it as `completed` and add any new follow-up tasks discovered during implementation\n\
        \n\
        ## When NOT to Use This Tool\n\
        Skip using this tool when:\n\
        1. There is only a single, straightforward task\n\
        2. The task is trivial and tracking it provides no organizational benefit\n\
        3. The task can be completed in less than 3 trivial steps\n\
        4. The task is purely conversational or informational\n\
        \n\
        NOTE: if there is only one trivial task to do, just do it directly — do not call this tool.\n\
        \n\
        ## Examples of When to Use the Task Tool\n\
        \n\
        <example>\n\
        User: I want to add a dark mode toggle to the application settings. Make sure you run the tests and build when you're done!\n\
        Assistant: *Calls task(action='create') five times, one per step:*\n\
          1. Create dark mode toggle component in Settings page\n\
          2. Add dark mode state management (context/store)\n\
          3. Implement CSS-in-JS styles for dark theme\n\
          4. Update existing components to support theme switching\n\
          5. Run tests and build, addressing any failures\n\
        *Then calls task(action='update', task_id='task-1', new_status='in_progress') and starts on the first step.*\n\
        \n\
        <reasoning>The assistant created tasks because: (1) it's a multi-step feature spanning UI, state, and styling; (2) the user explicitly asked for tests + build; (3) tracking lets the user see progress across all five steps.</reasoning>\n\
        </example>\n\
        \n\
        <example>\n\
        User: Help me rename the function getCwd to getCurrentWorkingDirectory across my project.\n\
        Assistant: *Uses grep to locate all occurrences first.* I found 15 instances across 8 files. *Creates one task per file plus a final 'verify with cargo check' task.*\n\
        \n\
        <reasoning>Search-then-plan: the assistant scoped the work first, then created tasks because the work crossed multiple files and needed systematic tracking to avoid missed instances.</reasoning>\n\
        </example>\n\
        \n\
        <example>\n\
        User: I need to implement these features for my e-commerce site: user registration, product catalog, shopping cart, and checkout flow.\n\
        Assistant: *Creates four parent tasks, one per feature, each with subtasks for db model + API + frontend.* Let's start with user registration.\n\
        \n\
        <reasoning>The user gave a comma-separated list of four large features — exactly the 'multiple tasks' trigger. Subtasks encode the sub-steps each feature needs.</reasoning>\n\
        </example>\n\
        \n\
        ## Examples of When NOT to Use the Task Tool\n\
        \n\
        <example>\n\
        User: How do I print 'Hello World' in Python?\n\
        Assistant: `print(\"Hello World\")`\n\
        \n\
        <reasoning>Single, trivial, informational. No tracking needed.</reasoning>\n\
        </example>\n\
        \n\
        <example>\n\
        User: Can you add a comment to the calculateTotal function?\n\
        Assistant: *Uses str_replace once.* Done.\n\
        \n\
        <reasoning>Single edit in one location — tracking adds noise.</reasoning>\n\
        </example>\n\
        \n\
        ## Task States and Management\n\
        \n\
        - **States**: `pending` → `in_progress` → `completed` (or `failed` / `cancelled`). The `deleted` action hard-removes a task.\n\
        - **Exactly one in_progress at a time**: flip the next task to `in_progress` only after marking the current one `completed`.\n\
        - **Mark complete IMMEDIATELY** after finishing — do not batch.\n\
        - **Add follow-ups as they appear**: if you discover new work mid-implementation, create new tasks rather than expanding existing ones.\n\
        \n\
        ## Field Conventions\n\
        - `title`: imperative, specific outcome (e.g. 'Fix auth redirect on Safari', not 'fix bug')\n\
        - `active_form`: present-continuous shown on the spinner while in_progress (e.g. 'Fixing auth redirect'). Omit → spinner shows title.\n\
        - `description`: what 'done' looks like. More detail helps if another agent might take over.\n\
        - `subtasks`: pre-plan nested work at create time; use `depends_on` to encode subtask order.\n\
        - `add_blocked_by [taskA, taskB]` → this task won't be next-actionable until A and B are done.\n\
        - `metadata`: free-form; later `update` with `{key: null}` deletes a specific key.\n\
        \n\
        ## Background Execution\n\
        - `background_shell`: run a shell command in the background while continuing to chat. Returns a task_id. Use for builds, tests, servers, long scripts.\n\
        - `background_agent`: spawn a durable sub-agent through the structured agent spawner. Use `get_agent_result` with the returned agent_id to collect output.\n\
        - `output`: read stdout/stderr from a background shell task. With `block: true` (default), waits up to `timeout_ms` for completion.\n\
        - `kill`: terminate a background task immediately.\n\
        - You will receive `<background_task_notification>` XML when background tasks complete, fail, or stall.\n\
        - If a task stalls (no output for 45s + looks like an interactive prompt), kill it and re-run with non-interactive flags.\n\
        \n\
        ## Tips\n\
        - Call `list` with `status_filter: 'active'` before creating new tasks to avoid dupes.\n\
        - Auto-completion: completing the last remaining subtask auto-completes the parent (only if parent is still active).\n\
        - Cascade: completing a parent cascades to pending/in_progress subtasks but preserves failed/cancelled.\n\
        - Rollback: task state is journaled per-turn; a /rollback undoes the most recent mutation batch.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "action": {"type": "string", "enum": ["create","update","list","get","stop","background_shell","background_agent","output","kill"], "description": "Operation to perform"},
                        "title": {"type": "string", "description": "(create/update) Brief imperative title"},
                        "description": {"type": "string", "description": "(create/update) What needs to be done"},
                        "task_id": {"type": "string", "description": "(update/get/stop) Task ID (e.g. 'task-1')"},
                        "new_status": {"type": "string", "enum": ["pending","in_progress","completed","failed","cancelled","deleted"], "description": "(update) New status to assign. 'deleted' permanently removes the task."},
                        "status": {"type": "string", "enum": ["pending","in_progress","completed","failed","cancelled","deleted"], "description": "(update, legacy alias for new_status) New status to assign."},
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
                        "error_message": {"type": "string", "description": "(update) Reason for failure"},
                        "command": {"type": "string", "description": "(background_shell) Shell command to run in background"},
                        "prompt": {"type": "string", "description": "(background_agent) Instruction for the background agent"},
                        "agent_type": {"type": "string", "enum": ["explore","code-review","task","general-purpose"], "description": "(background_agent) Agent type. Default general-purpose."},
                        "model": {"type": "string", "description": "(background_agent) Model override for the background agent"},
                        "block": {"type": "boolean", "description": "(output) Wait for task to complete before returning. Default true."},
                        "timeout_ms": {"type": "integer", "description": "(output) Max ms to wait when block=true. Default 30000, max 300000."}
                    },
                    "required": ["action"],
                    "x-astra-per-action-required": {
                        "create": ["title"],
                        "update": ["task_id"],
                        "get": ["task_id"],
                        "stop": ["task_id"],
                        "background_shell": ["command"],
                        "background_agent": ["prompt"],
                        "output": ["task_id"],
                        "kill": ["task_id"]
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

    // ── agent tool: sync-default + run_in_background contract ─────────────

    #[test]
    fn agent_schema_exposes_run_in_background_parameter() {
        // The TUI's `TaskCell` UX relies on the model being able to
        // opt out of the sync default when the user says "kick this
        // off in the background". Schema must surface the param
        // directly — a tool hint in the description is not enough
        // (cache budget) nor discoverable.
        let schemas = all_tool_schemas_with_env(|_| None);
        let agent = find_schema(&schemas, "agent").expect("agent schema must exist");
        let props = agent
            .get("function")
            .and_then(|f| f.get("parameters"))
            .and_then(|p| p.get("properties"))
            .expect("agent must expose parameters.properties");
        assert!(
            props.get("run_in_background").is_some(),
            "agent must expose `run_in_background` param so background delegation is discoverable"
        );
        assert_eq!(
            props
                .get("run_in_background")
                .and_then(|p| p.get("type"))
                .and_then(Value::as_str),
            Some("boolean"),
            "run_in_background must be typed as a boolean flag"
        );
    }

    #[test]
    fn agent_schema_description_documents_sync_default() {
        // Hard assertion that the tool description spells out the
        // sync-default contract — this is the behaviour that
        // separates astra's TaskCell UX from a fire-and-forget worker
        // queue. If a future refactor collapses the description
        // without the sync/async paragraph, the cache-safe short
        // description would lose the load-bearing semantics.
        let schemas = all_tool_schemas_with_env(|_| None);
        let agent = find_schema(&schemas, "agent").expect("agent schema must exist");
        let desc = agent
            .get("function")
            .and_then(|f| f.get("description"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        assert!(
            desc.contains("synchronous") || desc.contains("blocks until"),
            "agent description must state that spawn is sync by default"
        );
        assert!(
            desc.contains("run_in_background"),
            "agent description must name `run_in_background` so the model learns the opt-out"
        );
    }

    #[test]
    fn task_schema_uses_proactive_imperative_wording() {
        // The model only auto-decomposes a complex first-turn request
        // when the task tool's description carries claudecode-style
        // imperative language: "Use this tool proactively..." plus
        // worked <example> blocks. Soft "consider using" wording does
        // not fire reliably on turn 0.
        //
        // This test pins the load-bearing phrases so a future "let's
        // shorten the description" refactor cannot silently regress
        // the auto-decompose behaviour without tripping the test.
        let schemas = all_tool_schemas_with_env(|_| None);
        let task = find_schema(&schemas, "task").expect("task schema must exist");
        let desc = task
            .get("function")
            .and_then(|f| f.get("description"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        assert!(
            desc.contains("proactively"),
            "task description must say 'proactively' — soft 'consider' wording does not \
             trigger turn-0 decomposition. Got: {desc}"
        );
        assert!(
            desc.contains("3 or more distinct steps") || desc.contains("3+ distinct"),
            "task description must name the explicit '3+ steps' threshold so the model has \
             a hard trigger, not a fuzzy heuristic"
        );
        assert!(
            desc.contains("<example>"),
            "task description must include worked <example> blocks — the model imitates \
             demonstrated patterns far more reliably than abstract advice"
        );
        assert!(
            desc.contains("BEFORE beginning work") || desc.contains("BEFORE you begin"),
            "task description must enforce 'mark in_progress BEFORE work' or the spinner \
             never shows real-time status"
        );
        assert!(
            desc.contains("ONE task as `in_progress`")
                || desc.contains("one in_progress at a time"),
            "task description must enforce single-active to prevent the model from \
             flipping every task to in_progress at once"
        );
    }

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

    #[cfg(unix)]
    #[test]
    fn server_run_script_schema_lists_only_server_routable_rpc_tools() {
        let schemas = server_executor_tool_schemas();
        let rs = schemas
            .iter()
            .find(|s| {
                s.get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(Value::as_str)
                    == Some("run_script")
            })
            .expect("server run_script schema present");
        let desc = rs["function"]["description"].as_str().unwrap();
        for name in SERVER_RUN_SCRIPT_RPC_TOOL_NAMES {
            assert!(
                desc.contains(name),
                "server run_script schema should mention routable RPC tool `{name}`"
            );
        }
        assert!(
            !desc.contains("search_files") && !desc.contains("patch"),
            "server run_script must not advertise RPC tools not routed by ServerToolExecutor"
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
