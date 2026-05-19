//! Tool schema definitions for all edge tools.
//
//! Each schema is a JSON object following the OpenAI function-calling format:
//! `{ "type": "function", "function": { "name": ..., "description": ..., "parameters": ... } }`

use serde_json::{Value, json};

/// RPC tools exposed inside server-side `run_script`.
///
/// This is intentionally narrower than [`crate::run_script::RPC_ALLOWED_TOOLS`]:
/// the web/API server must only advertise sub-tools that the
/// `ServerToolExecutor` can actually route in-process.
pub const SERVER_RUN_SCRIPT_RPC_TOOL_NAMES: &[&str] = &[
    "read_file",
    "write_file",
    "list_dir",
    "grep",
    "web_fetch",
    "bash",
];

pub fn all_tool_schemas() -> Vec<Value> {
    all_tool_schemas_with_env(|k| std::env::var(k).ok())
}

/// Replace the `run_script` schema with the narrowed server-side variant.
#[cfg(unix)]
pub fn narrow_run_script_for_server(schemas: &mut [Value]) {
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
                "description": "Execute a shell command in the project root. Use for builds, tests, installs, and CLI tasks; prefer dedicated read/search/edit/git tools when possible.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "command": {"type": "string", "description": "Shell command to run"},
                        "timeout": {"type": "number", "description": "Timeout in seconds (default 120). Use a larger value for long builds/tests, e.g. cargo build or full test suites."},
                        "force": {"type": "boolean", "description": "Bypass the per-session identical-command cache."}
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
                "description": "Create, overwrite, or delete a file. For writes, provide `path` and `content`. For deletes, set `delete=true` and omit `content`. Retry `write_file` with corrected args; do not switch to bash or python just to write a file.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "File path relative to project root"},
                        "content": {"type": "string", "description": "File content. Required unless deleting."},
                        "delete": {"type": "boolean", "description": "Delete instead of write. Omit content when true."}
                    },
                    "required": ["path"],
                    "x-astra-per-action-required": {
                        "write": ["path", "content"],
                        "delete": ["path"]
                    }
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "str_replace",
                "description": "Replace text in a file. Supports single replacement or batched `edits`.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "File path relative to project root"},
                        "old_str": {"type": "string", "description": "String to replace (single-edit mode)."},
                        "new_str": {"type": "string", "description": "Replacement text (single-edit mode)."},
                        "edits": {
                            "type": "array",
                            "description": "Atomic array of {old_str, new_str} pairs. Mutually exclusive with old_str/new_str.",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "old_str": {"type": "string"},
                                    "new_str": {"type": "string"}
                                },
                                "required": ["old_str", "new_str"]
                            }
                        },
                        "dry_run": {"type": "boolean", "description": "Preview without applying."},
                        "replace_all": {"type": "boolean", "description": "Replace all occurrences."},
                        "allow_structural_change": {"type": "boolean", "description": "Bypass structural safety checks for intentional syntax-breaking edits."}
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
                "description": "Search file contents with a regex pattern. Respects .gitignore.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "pattern": {"type": "string", "description": "Regex pattern to search for"},
                        "path": {"type": "string", "description": "Directory or file to search."},
                        "include": {"type": "string", "description": "Optional file glob filter, e.g. '*.rs'."},
                        "case_sensitive": {"type": "boolean", "description": "Case-sensitive search."},
                        "fixed_strings": {"type": "boolean", "description": "Treat pattern as a literal string."},
                        "max_matches": {"type": "integer", "description": "Max matches per file"},
                        "output_mode": {"type": "string", "enum": ["content", "files_with_matches", "count"], "description": "content, files_with_matches, or count."}
                    },
                    "required": ["pattern"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "glob",
                "description": "Find files matching a glob pattern. Supports pagination via offset/head_limit.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "pattern": {"type": "string", "description": "Glob pattern e.g. '**/*.rs'"},
                        "path": {"type": "string", "description": "Root directory."},
                        "sort_by": {"type": "string", "enum": ["mtime", "path"], "description": "Sort by newest mtime or by path."},
                        "offset": {"type": "integer", "minimum": 0, "description": "Skip first N matching files (for pagination)"},
                        "head_limit": {"type": "integer", "minimum": 0, "description": "Max files after offset. Default 100; 0 = unlimited."}
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
                "description": "Memory operations: `remember` (store), `recall` (search), `expand` (open by id), `forget` (soft-delete), `update` (correct), `focus` (temporary recall boost), `reflect` (synthesize patterns), `profile` (user profile), `feedback` (mark quality). `visibility=\"team\"` shares within a team; `agent_type` scopes by persona.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "action": {
                            "type": "string",
                            "enum": ["remember","recall","expand","forget","update","focus","reflect","profile","feedback"],
                            "description": "Memory operation."
                        },
                        "content": {"type": "string", "description": "Fact to store or replacement content."},
                        "query": {"type": "string", "description": "Search query or update selector."},
                        "memory_id": {"type": "string", "description": "Target memory id."},
                        "memory_type": {
                            "type": "string",
                            "enum": ["semantic","profile","procedural","working","episodic"],
                            "description": "Memory category."
                        },
                        "top_k": {"type": "integer", "description": "Max results."},
                        "min_confidence": {"type": "number", "description": "Confidence filter (0.0-1.0)."},
                        "scope": {
                            "type": "string",
                            "enum": ["all","session"],
                            "description": "Recall scope."
                        },
                        "view": {
                            "type": "string",
                            "enum": ["compact","overview","full"],
                            "description": "Recall detail level."
                        },
                        "importance": {"type": "number", "description": "Importance score."},
                        "trust_tier": {"type": "string", "description": "Trust tier."},
                        "tags": {"type": "array", "items": {"type": "string"}, "description": "Tags."},
                        "tags_add": {"type": "array", "items": {"type": "string"}, "description": "Tags to add."},
                        "tags_remove": {"type": "array", "items": {"type": "string"}, "description": "Tags to remove."},
                        "visibility": {
                            "type": "string",
                            "enum": ["private","team"],
                            "description": "Visibility."
                        },
                        "team_id": {
                            "type": "string",
                            "description": "Team id for team visibility."
                        },
                        "reason": {"type": "string", "description": "Audit reason."},
                        "level": {
                            "type": "string",
                            "enum": ["abstract","overview","detail","linked"],
                            "description": "expand depth."
                        },
                        "focus_type": {
                            "type": "string",
                            "enum": ["topic","tag","memory_id","session"],
                            "description": "focus target type."
                        },
                        "focus_value": {"type": "string", "description": "Focus target value."},
                        "boost": {"type": "number", "description": "Boost multiplier."},
                        "ttl_secs": {"type": "integer", "description": "Boost TTL seconds."},
                        "signal": {
                            "type": "string",
                            "enum": ["useful","irrelevant","outdated","wrong"],
                            "description": "feedback quality signal."
                        },
                        "context": {"type": "string", "description": "Optional context."},
                        "agent_type": {
                            "type": "string",
                            "enum": ["explore","code-review","task","general-purpose"],
                            "description": "Persona scope."
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
                "description": "Session lifecycle and introspection. Actions: config, prioritize, deprioritize, set_goal, compact, rollback_edits, sleep, timeline, summary, history_page, history_search, history_around. Use the first-class `ask_user` tool for user questions. For plan mode use the dedicated `enter_plan_mode` / `exit_plan_mode` tools — they're not session sub-actions any more. Use the history_* actions when the user refers to older turns in this same chat and the visible context is insufficient.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "action": {"type": "string", "enum": ["config","prioritize","deprioritize","set_goal","compact","rollback_edits","sleep","timeline","summary","history_page","history_search","history_around"]},
                        "key": {"type": "string", "description": "Config key"},
                        "value": {"type": "string", "description": "Config value"},
                        "tool": {"type": "string", "description": "Tool name (prioritize/deprioritize)"},
                        "goal": {"type": "string", "description": "Goal text"},
                        "scope": {"type": "string", "enum": ["current_turn","turn","file","list"], "description": "Rollback scope"},
                        "path": {"type": "string", "description": "File path (rollback scope=file)"},
                        "turn_index": {"type": "integer", "description": "Turn index (rollback scope=turn)"},
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
         ## Required fields per action\n\
         - `spawn`: REQUIRES `action`, `description`, `prompt`. (Optional: `agent_type`, `run_in_background`, `model`, `max_turns`, `complexity`, `isolated`, `allowed_tools`, `name`.)\n\
         - `get_result`: REQUIRES `action`, `agent_id`.\n\
         - `run_chain`: REQUIRES `action`, `steps`.\n\
         - `send_message`: REQUIRES `action`, `to`, `message`.\n\n\
         For `spawn`, pass at least one non-empty field: `description` (short UI summary) or `prompt` (full child brief). If one is missing, Astra derives it from the other. Prefer sending both. Do NOT pass a top-level `task` field. Do NOT pass `agent_id` to spawn; Astra generates that runtime id for you. If you need a mailbox label, use `name`, but `name` is not valid for `get_result`.\n\n\
         ## Spawn example\n\
         `agent(action='spawn', description='Audit auth flow', prompt='Read src/auth/* and report any token-handling bugs. Focus on session expiry and refresh logic. Return findings as a numbered list.', agent_type='general-purpose')`\n\n\
         ## Execution mode\n\
         - **Default (synchronous)**: `spawn` blocks until the sub-agent's final result is ready. Use this for work you depend on in the current turn. The sub-agent's tool calls stream back inline — the TUI renders them inside the parent Task card so the user sees progress live.\n\
         - **Background**: pass `run_in_background: true` to return immediately with `{agent_id}`. Use this for fire-and-forget or long-running work you don't need to await; follow up with `get_result` later using the exact returned `agent_id`. Durable-task store persists the run across session death so it survives `astra` restarts.\n\n\
         ## Parallel sub-agent fan-out\n\
         To run N sub-agents in parallel (e.g. multi-angle code review), emit N `agent` tool calls **in a single assistant message**, each with `action='spawn'` and `run_in_background: true`. They run concurrently. After all are spawned, capture each spawn result's returned `agent_id`, then call `agent(action='get_result', agent_id=...)` with that exact value for each one — `get_result` blocks until that child finishes. Do not substitute `name` or invent ids. This is the ONLY way to fan out parallel agents; do not use `action='delegate'` (removed: it had no execution backend).\n\
         For plan lifecycle, call `enter_plan_mode` / `exit_plan_mode` directly. Do NOT wrap them inside `agent(action='run_chain', ...)`.\n\
         Do NOT pass an `agents:[...]` payload, do NOT pass a top-level `task` field, and do NOT wrap spawn arguments under a `spawn` field. Each child must be its own `agent(...)` tool call.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "action": {"type": "string", "enum": ["spawn","get_result","run_chain","send_message"]},
                        "steps": {"type": "array", "description": "REQUIRED for action='run_chain'. Sequence of chain steps to execute."},
                        "description": {"type": "string", "description": "Spawn summary shown in the UI Task card. Short, specific, non-empty."},
                        "prompt": {"type": "string", "description": "Full child task brief for spawn. Non-empty. If omitted, Astra falls back to `description`."},
                        "agent_type": {"type": "string", "enum": ["explore","code-review","task","general-purpose"], "description": "Sub-agent persona (spawn). Default: general-purpose."},
                        "model": {"type": "string", "description": "Model override (spawn). Default: parent's model."},
                        "run_in_background": {"type": "boolean", "description": "If true, return immediately with a runtime-generated agent_id instead of blocking on the sub-agent's final result. Use that exact returned value with get_result. Default false (sync). Applies to spawn."},
                        "name": {"type": "string", "description": "Addressable mailbox name (spawn). Optional; auto-generated if omitted. Not the runtime agent_id used by get_result."},
                        "max_turns": {"type": "integer", "description": "Max turns (spawn). Explicit value wins over `complexity`."},
                        "complexity": {"type": "string", "enum": ["light","normal","deep"], "description": "Task-complexity hint scaling the default budget when `max_turns` is absent. `light`≈10 turns, `normal`=agent default, `deep`=2× default. Use `deep` for review/refactor/multi-file tasks that routinely exhaust the default."},
                        "isolated": {"type": "boolean", "description": "Use isolated worktree (spawn)"},
                        "allowed_tools": {"type": "array", "items": {"type": "string"}, "description": "Tool allowlist (spawn)"},
                        "agent_id": {"type": "string", "description": "REQUIRED for action='get_result'. Must be the exact runtime-generated agent_id returned by a prior spawn, not the optional spawn name."},
                        "to": {"type": "string", "description": "REQUIRED for action='send_message'. Recipient agent_id, or '*' for broadcast."},
                        "message": {"description": "REQUIRED for action='send_message'. Message content."},
                        "message_type": {"type": "string", "enum": ["text","question","answer","instruction","progress","result","shutdown_request","shutdown_response"]},
                        "priority": {"type": "string", "enum": ["low","normal","high"]}
                    },
                    "required": ["action"],
                    "additionalProperties": false,
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
                "description": "Query runtime state. Subtopics: `session` (default: token pressure, cache hit rate, tool health, alerts, working memory, and plan/task/session lifecycle context including restore/resume state and last lifecycle event when available), `cache` (cache-regression diagnosis), `recent` (recent LLM-round summaries), `volatile` (runtime nudges / working-set / coaching queued for next turn), `stall` (loop-guard state), `all` (session + recent + volatile + stall).",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "subtopic": {"type": "string", "enum": ["session","cache","recent","volatile","stall","noise","all"], "description": "Diagnostic to run. `noise` reports stale runtime-injected prompt channels."},
                        "detail": {"type": "string", "enum": ["full","summary","minimal"], "description": "Detail level for session output."}
                    }
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "tool_search",
                "description":
                    "Search deferred tools. Use keyword queries to find matches, or \
                     `query=\"select:NAME\"` / `select:NAME1,NAME2` to fetch full schemas \
                     for specific deferred tools. After `select:`, invoke the chosen tool \
                     directly on the next turn.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description":
                                "Keyword query, or `select:NAME` / `select:NAME1,NAME2`."
                        },
                        "max_results": {
                            "type": "integer",
                            "description": "Keyword-mode result limit (default 5, max 20)."
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
                "description": "Ask the user a structured questionnaire when you need clarification or a decision. In TUI Prompt mode this opens a native tabbed overlay and pauses until the user answers. ALWAYS send a top-level `questions` array; do not send legacy top-level fields like `question` or `choices`. If an ask_user call fails because the payload shape is wrong, fix the questionnaire and retry ask_user immediately — do not continue implementation without the clarification. Prefer 1-6 focused questions in `questions[]`. Each question should have a short header for the tab chip, a clear question, and usually 2-9 options. For a pure freeform question, you may omit options and leave allow_freeform=true. Set multi_select=true when choices are not mutually exclusive. Single-select questions may optionally attach preview text to options; the UI will show a side-by-side preview panel for the focused option. Do not include an Other option because the UI adds freeform input automatically when allow_freeform is true. Put the recommended option first and include '(Recommended)' in its label when applicable. Example: {\"questions\":[{\"header\":\"Scope\",\"question\":\"Which scope should we ship first?\",\"options\":[\"Core flow\",\"Full workflow\"],\"allow_freeform\":true}]}. Use sparingly — only when the next step is genuinely ambiguous.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "context": {"type": "string", "description": "Brief context shown above the questionnaire (dimmed)"},
                        "questions": {
                            "type": "array",
                            "description": "1-6 questions to present in the ask_user questionnaire.",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "header": {"type": "string", "description": "Very short tab label, e.g. 'Frontend' or 'Database'. If omitted, the UI derives one from the question."},
                                    "question": {"type": "string", "description": "The focused question to ask for this tab."},
                                    "options": {"type": "array", "items": {"anyOf": [
                                        {"type": "string"},
                                        {"type": "object", "properties": {
                                            "label": {"type": "string", "description": "Option label shown in the picker"},
                                            "description": {"type": "string", "description": "Short explanatory text shown next to the option"},
                                            "preview": {"type": "string", "description": "Optional preview text shown in a side-by-side preview panel for single-select questions."}
                                        }, "required": ["label"]}
                                    ]}, "description": "Usually 2-9 options for this question. Do not include Other; use allow_freeform. May be omitted for a pure freeform question."},
                                    "multi_select": {"type": "boolean", "description": "Whether the user may select multiple options for this question."},
                                    "allow_freeform": {"type": "boolean", "description": "Whether the UI should add an automatic Other/freeform path for this question. Defaults to true."}
                                },
                                "required": ["question"]
                            }
                        }
                    },
                    "required": ["questions"]
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
                "description": "Durable task list. Use this tool proactively for multi-step work and progress.\n\
        \n\
        Actions: create, update, list, get, stop, list_user, adopt, archive. Checklist only — use `agent_job` for background shell/sub-agent work.\n\
        \n\
        ## When to Use\n\
        - 3 or more distinct outcomes, files, phases, or deliverables.\n\
        - Approved plan execution or delegated/background work.\n\
        - Scope expands mid-flight.\n\
        \n\
        When tracking is useful:\n\
        1. Create one task per concrete outcome or phase — NOT one umbrella task for the whole request.\n\
        2. For broad work, split into 3-7 leaf tasks sized to one artifact, API surface, or validation step.\n\
        3. Mark the first actionable task as `in_progress` BEFORE beginning work.\n\
        4. Keep exactly ONE task as `in_progress` at a time.\n\
        5. Mark tasks completed immediately; on failure set `failed` with `error_message`.\n\
        \n\
        ## When NOT to Use\n\
        - Single edit / single command / answer.\n\
        - Pure information request.\n\
        - Trivial work.\n\
        \n\
        ## Field Conventions\n\
        - `title`: specific outcome.\n\
        - `active_form`: spinner text while in_progress.\n\
        - `description`: what done looks like.\n\
        - `subtasks`: optional nested steps; use `depends_on` for order.\n\
        - `metadata`: free-form state; on update, `{key: null}` deletes that key.\n\
        \n\
        - `list_user` shows cross-session tasks; `adopt` copies one into the current session.\n\
        \n\
        <example>\n\
        User: Build an employee reimbursement system.\n\
        Assistant: Create separate tasks like `scaffold backend`, `implement expense API`, `build frontend flows`, `verify startup`; do NOT create one umbrella task `build reimbursement system`. Mark the first task `in_progress` BEFORE beginning work.\n\
        </example>",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "action": {"type": "string", "enum": ["create","update","list","get","stop","list_user","adopt","archive"], "description": "Operation to perform"},
                        "source_session_id": {"type": "string", "description": "(adopt) Source session id."},
                        "older_than_days": {"type": "integer", "description": "(archive bulk) Archive completed tasks older than N days. Default 30."},
                        "user_status": {"type": "string", "enum": ["active","completed","failed","all"], "description": "(list_user) Cross-session filter. Default active."},
                        "title": {"type": "string", "description": "(create/update) Imperative title."},
                        "description": {"type": "string", "description": "(create/update) Definition of done."},
                        "task_id": {"type": "string", "description": "(update/get/stop/adopt/archive) Task id."},
                        "new_status": {"type": "string", "enum": ["pending","in_progress","completed","failed","cancelled","deleted"], "description": "(update) New status. `deleted` permanently removes the task."},
                        "status": {"type": "string", "enum": ["pending","in_progress","completed","failed","cancelled","deleted"], "description": "(update) Legacy alias for new_status."},
                        "status_filter": {"type": "string", "enum": ["pending","in_progress","completed","failed","all","active"], "description": "(list) Result filter. `active` = pending + in_progress."},
                        "subtask_id": {"type": "string", "description": "(update) Specific subtask id."},
                        "active_form": {"type": "string", "description": "(create/update) Spinner text while in_progress."},
                        "owner": {"type": "string", "description": "(create/update) Task owner."},
                        "metadata": {"type": "object", "description": "(create/update) Arbitrary key-value pairs; null deletes a key on update."},
                        "add_blocks": {"type": "array", "items": {"type": "string"}, "description": "(update) Task ids blocked by this task."},
                        "add_blocked_by": {"type": "array", "items": {"type": "string"}, "description": "(update) Task ids that must finish before this task starts."},
                        "remove_blocks": {"type": "array", "items": {"type": "string"}, "description": "(update) Remove entries from blocks."},
                        "remove_blocked_by": {"type": "array", "items": {"type": "string"}, "description": "(update) Remove entries from blocked_by."},
                        "subtasks": {
                            "type": "array",
                            "description": "(create) Optional subtasks.",
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
                        "reason": {"type": "string", "description": "(stop) Why the task is being stopped."},
                        "error_message": {"type": "string", "description": "(update) Failure reason."}
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
        // ── agent_job ───────────────────────────────────────────────
        // Background execution surface. Owns shell processes and
        // durable sub-agent runs that the model wants to fire-and-
        // poll. Split out of `task` in 2026-05 so the model has one
        // tool for the session checklist and a different tool for
        // long-running work — see `task_schema_does_not_advertise_
        // background_actions`. Inspiration: codex `spawn_agents_on_csv`
        // + `report_agent_job_result`; claudecode `Bash(run_in_background)`
        // + `Agent(run_in_background)`.
        json!({
            "type": "function",
            "function": {
                "name": "agent_job",
                "description": "Run shell commands or spawn durable sub-agents in the background while continuing to chat. Use this when work is long-running, can run independently, or you need to do other things in parallel.\n\
        \n\
        Actions: shell, agent, output, kill.\n\
        \n\
        ## When to Use This Tool\n\
        - **Long-running shell** (builds, test suites, servers, scripts > ~10s): use `shell` instead of blocking `bash` — keeps the conversation responsive.\n\
        - **Durable sub-agent fan-out**: use `agent` to spawn an agent that should survive until it produces a result. The job ID returned is durable across CLI restart.\n\
        - **Need output later**: pair `shell` / `agent` with a follow-up `output` call. With `block: true` (default), `output` waits up to `timeout_ms` for completion. Use `offset` to resume from the last byte you already consumed.\n\
        - **Cancel a stuck job**: `kill` terminates immediately.\n\
        \n\
        ## When NOT to Use This Tool\n\
        - Quick commands (< 5s): use `bash` directly — the round-trip overhead isn't worth it.\n\
        - Synchronous sub-agent that you need the answer from before continuing: use `agent(action='spawn', ...)` + `agent(action='get_result', agent_id=...)` — that path is integrated with the parallel-spawn coalescing window.\n\
        - In-session todos/checklist tracking: use `task` (create/update/list/get/stop). `agent_job` is for processes, not progress markers.\n\
        \n\
        ## Notifications\n\
        You will receive `<background_task_notification>` XML when background jobs complete, fail, or stall (no output for ~45s + interactive-prompt pattern). When you see one, decide whether to read its output, acknowledge it, or `kill` and retry with non-interactive flags.\n\
        \n\
        ## Examples\n\
        \n\
        <example>\n\
        User: kick off the full test suite, I'll keep working.\n\
        Assistant: *Calls agent_job(action='shell', command='cargo test --workspace')* — returns task_id `bg-shell-3`. *Continues with other work; later calls agent_job(action='output', task_id='bg-shell-3') to read the results.*\n\
        </example>\n\
        \n\
        <example>\n\
        User: have an explorer agent map every TODO across the codebase while we keep coding.\n\
        Assistant: *Calls agent_job(action='agent', prompt='Find every TODO/FIXME...', agent_type='explore')* — fires it in the background.\n\
        </example>",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "action": {
                            "type": "string",
                            "enum": ["shell", "agent", "output", "kill"],
                            "description": "Operation to perform"
                        },
                        "command": {
                            "type": "string",
                            "description": "(shell) Shell command to run in the background"
                        },
                        "prompt": {
                            "type": "string",
                            "description": "(agent) Instruction for the background sub-agent"
                        },
                        "agent_type": {
                            "type": "string",
                            "enum": ["explore", "code-review", "task", "general-purpose"],
                            "description": "(agent) Agent type. Default general-purpose."
                        },
                        "model": {
                            "type": "string",
                            "description": "(agent) Model override for the background agent"
                        },
                        "task_id": {
                            "type": "string",
                            "description": "(output/kill) The background job ID returned by shell/agent"
                        },
                        "block": {
                            "type": "boolean",
                            "description": "(output) Wait for the job to complete before returning. Default true."
                        },
                        "offset": {
                            "type": "integer",
                            "minimum": 0,
                            "description": "(output) Resume reading from this byte offset. Default 0."
                        },
                        "max_bytes": {
                            "type": "integer",
                            "minimum": 1,
                            "description": "(output) Maximum bytes to return from the current offset. Default 8192, max 65536."
                        },
                        "timeout_ms": {
                            "type": "integer",
                            "description": "(output) Max ms to wait when block=true. Default 30000, max 300000."
                        }
                    },
                    "required": ["action"],
                    "x-astra-per-action-required": {
                        "shell": ["command"],
                        "agent": ["prompt"],
                        "output": ["task_id"],
                        "kill": ["task_id"]
                    }
                }
            }
        }),
        // ── enter_plan_mode ─────────────────────────────────────────
        // Top-level sentinel tool that flips the session into plan
        // mode. Promoted from the buried `session.enter_plan` action
        // in 2026-05 because the model rarely picked the sub-action
        // — claudecode's dedicated `EnterPlanMode` tool is the
        // reference. While in plan mode, write tools (str_replace,
        // write_file, bash, git commit, …) are denied at the
        // permission gate; read tools stay available for codebase
        // exploration. Exit via `exit_plan_mode` — that's the only
        // unlock path.
        json!({
            "type": "function",
            "function": {
                "name": "enter_plan_mode",
                "description": "Enter plan mode for non-trivial work that needs design before code. While in plan mode you can ONLY read the codebase (read_file, grep, glob, list_dir, symbols, web_fetch). Edits, shell commands, and git mutations are blocked at the permission gate — author the plan, then call `exit_plan_mode` with the markdown for user approval.\n\
        \n\
        ## When to Use This Tool\n\
        Use plan mode when user alignment before edits materially reduces risk:\n\
        - Multiple reasonable implementation approaches exist and the choice affects architecture, data flow, permissions, public API, or persistence.\n\
        - Requirements are unclear enough that exploration should precede a concrete implementation proposal.\n\
        - The work is high-impact or hard to unwind, such as schema changes, auth/security behavior, cross-cutting refactors, or large migrations.\n\
        - The user explicitly wants a plan, design review, or approval before implementation.\n\
        \n\
        When you enter plan mode:\n\
        1. Edits are blocked by design.\n\
        2. Explore with read tools and identify existing patterns to follow.\n\
        3. Produce executable leaf steps: each step should map to one concrete artifact, API surface, or validation target.\n\
        4. Avoid umbrella steps like \"build the whole system\" when code, API, UI, and verification are separate outcomes.\n\
        5. Call `exit_plan_mode(plan='<markdown>', approved=true)` to surface the plan for user approval. Approval unlocks edits and seeds executable plan items.\n\
        \n\
        ## When NOT to Use This Tool\n\
        Do not enter plan mode when normal execution is clearer:\n\
        - Single-line / few-line fixes (typos, obvious bugs).\n\
        - User gave specific step-by-step instructions — just do them.\n\
        - Pure research / read-only exploration with no implementation step (use `agent` with explore type instead).\n\
        - The work is < 3 files and the approach is obvious.\n\
        \n\
        Important: `exit_plan_mode` is the ONLY way to leave plan mode. Do not use `ask_user` to ask \"is the plan ready?\" — `exit_plan_mode` itself surfaces the plan for approval.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "goal": {
                            "type": "string",
                            "description": "Optional one-line goal label that surfaces in the TUI plan-mode banner. Defaults to a placeholder if omitted."
                        }
                    },
                    "required": []
                }
            }
        }),
        // ── exit_plan_mode ──────────────────────────────────────────
        // Companion to enter_plan_mode. Surfaces the proposed plan to
        // the user for approval, lifts the write-tool guard on
        // success, and (server-side) seeds the approved plan items
        // into `session_plan_todos` so the next turn can execute
        // step-by-step.
        json!({
            "type": "function",
            "function": {
                "name": "exit_plan_mode",
                "description": "Present the plan for user approval and exit plan mode. The `plan` argument is a markdown string (numbered list, nested bullets ok) that the user reads and either approves or rejects. On approval, write tools unlock and the items seed `session_plan_todos`. On rejection (`approved=false`), the plan stays open for another authoring pass.\n\
        \n\
        ## Important\n\
        - Do NOT call this tool to ask 'is the plan ready?' — that's exactly what THIS tool does. It inherently requests approval.\n\
        - Pass the FULL plan as a single markdown string in `plan`. The user sees this verbatim.\n\
        - Prefer executable leaf steps over umbrella phases so approval seeds actionable tasks instead of one coarse catch-all item.\n\
        - Only call this when the plan is concrete and unambiguous. If you have unresolved decisions, use `ask_user` first.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "plan": {
                            "type": "string",
                            "description": "The plan markdown to present for approval. Numbered list of steps; nested bullets ok. The user reads this verbatim."
                        },
                        "approved": {
                            "type": "boolean",
                            "description": "True (default) to commit the plan and unlock writes. False to keep planning."
                        }
                    },
                    "required": ["plan"]
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

    fn schema_token_cost(schema: &Value) -> usize {
        serde_json::to_string(schema)
            .expect("schema must serialize")
            .len()
            .div_ceil(4)
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
    fn agent_schema_parallel_fanout_warns_against_agents_payloads() {
        let schemas = all_tool_schemas_with_env(|_| None);
        let agent = find_schema(&schemas, "agent").expect("agent schema must exist");
        let desc = agent
            .get("function")
            .and_then(|f| f.get("description"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        assert!(
            desc.contains("Do NOT pass") || desc.contains("do not pass"),
            "agent description must explicitly forbid the common unsupported wrapper/payload shapes"
        );
        assert!(
            desc.contains("agents"),
            "agent description must name the unsupported `agents` payload so the model stops retrying it"
        );
        assert!(
            desc.contains("exit_plan_mode") && desc.contains("run_chain"),
            "agent description should steer plan lifecycle away from run_chain"
        );
    }

    #[test]
    fn agent_schema_pins_exact_runtime_agent_id_contract() {
        let schemas = all_tool_schemas_with_env(|_| None);
        let agent = find_schema(&schemas, "agent").expect("agent schema must exist");
        let desc = agent
            .get("function")
            .and_then(|f| f.get("description"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        let params = &agent["function"]["parameters"];
        assert_eq!(params["additionalProperties"], false);
        assert!(
            desc.contains("Do NOT pass `agent_id` to spawn"),
            "agent description must explicitly forbid spawn-time agent_id misuse"
        );
        assert!(
            desc.contains("exact returned `agent_id`") || desc.contains("exact value"),
            "agent description must require reusing the returned runtime agent_id"
        );
        assert!(
            params["properties"]["name"]["description"]
                .as_str()
                .unwrap_or("")
                .contains("Not the runtime agent_id"),
            "name field must say it is not the get_result identifier"
        );
    }

    #[test]
    fn agent_job_schema_uses_consolidated_agent_actions() {
        let schemas = all_tool_schemas_with_env(|_| None);
        let agent_job = find_schema(&schemas, "agent_job").expect("agent_job schema must exist");
        let desc = agent_job
            .get("function")
            .and_then(|f| f.get("description"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        assert!(
            desc.contains("agent(action='spawn', ...)")
                && desc.contains("agent(action='get_result', agent_id=...)"),
            "agent_job description must teach the consolidated agent(action=...) syntax"
        );
        assert!(
            !desc.contains("agent.spawn") && !desc.contains("agent.get_result"),
            "agent_job description must not mention the legacy dotted agent syntax"
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
            desc.contains("3 or more distinct steps")
                || desc.contains("3 or more distinct outcomes")
                || desc.contains("3+ distinct"),
            "task description must name the explicit '3+ outcomes/steps' threshold so the model has \
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
    fn memory_and_task_schemas_stay_compact() {
        let schemas = all_tool_schemas_with_env(|_| None);
        let memory = find_schema(&schemas, "memory").expect("memory schema must exist");
        let task = find_schema(&schemas, "task").expect("task schema must exist");
        let memory_tokens = schema_token_cost(memory);
        let task_tokens = schema_token_cost(task);

        assert!(
            memory_tokens <= 700,
            "memory schema regressed to {memory_tokens} tokens; keep it compact"
        );
        assert!(
            task_tokens <= 1100,
            "task schema regressed to {task_tokens} tokens; keep it compact"
        );
    }

    #[test]
    fn task_schema_discourages_single_umbrella_task() {
        let schemas = all_tool_schemas_with_env(|_| None);
        let task = find_schema(&schemas, "task").expect("task schema must exist");
        let desc = task["function"]["description"].as_str().unwrap();

        assert!(
            desc.contains("umbrella task"),
            "task schema should explicitly forbid one giant catch-all task: {desc}"
        );
        assert!(
            desc.contains("3-7 leaf tasks") || desc.contains("separate tasks"),
            "task schema should steer complex work toward multiple actionable tasks: {desc}"
        );
    }

    #[test]
    fn task_schema_exposes_lifecycle_progress_and_dependencies() {
        let schemas = all_tool_schemas_with_env(|_| None);
        let task = find_schema(&schemas, "task").expect("task schema must exist");
        let properties = &task["function"]["parameters"]["properties"];

        for field in ["active_form", "add_blocks", "add_blocked_by"] {
            assert!(
                properties.get(field).is_some(),
                "task schema must expose {field} to the model"
            );
        }
        assert!(
            properties["active_form"]["description"]
                .as_str()
                .unwrap_or_default()
                .contains("Spinner text"),
            "active_form should stay product-facing spinner guidance"
        );
    }

    #[test]
    fn plan_and_task_schemas_use_semantic_guidance_not_lexical_triggers() {
        let schemas = all_tool_schemas_with_env(|_| None);
        let task = find_schema(&schemas, "task").expect("task schema must exist");
        let enter =
            find_schema(&schemas, "enter_plan_mode").expect("enter_plan_mode schema must exist");
        let task_desc = task["function"]["description"].as_str().unwrap();
        let plan_desc = enter["function"]["description"].as_str().unwrap();

        for desc in [task_desc, plan_desc] {
            assert!(
                desc.contains("## When to Use") && desc.contains("## When NOT to Use"),
                "tool UX guidance should be semantic and example-driven, not an activation-rule matcher: {desc}"
            );
            assert!(
                !desc.contains("ACTIVATION RULE"),
                "tool schema must not expose matcher-style activation rules: {desc}"
            );
            assert!(
                !desc.contains("conjunctions like"),
                "task schema must not teach lexical trigger matching: {desc}"
            );
            assert!(
                !desc.contains("\"what's the best way to\"") && !desc.contains("\"redesign\""),
                "plan schema must not encode phrase-list triggers: {desc}"
            );
        }
    }

    #[test]
    fn introspect_schema_mentions_lifecycle_and_resume_state() {
        let schemas = all_tool_schemas_with_env(|_| None);
        let introspect = find_schema(&schemas, "introspect").expect("introspect schema must exist");
        let desc = introspect["function"]["description"]
            .as_str()
            .expect("introspect description must be a string");
        assert!(
            desc.contains("plan/task/session lifecycle context"),
            "introspect should advertise lifecycle visibility: {desc}"
        );
        assert!(
            desc.contains("restore/resume state"),
            "introspect should advertise resume visibility: {desc}"
        );
        assert!(
            desc.contains("last lifecycle event"),
            "introspect should advertise causal last-event visibility: {desc}"
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
        let mut schemas = all_tool_schemas();
        narrow_run_script_for_server(&mut schemas);
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
    fn write_file_schema_requires_content_or_delete_contract() {
        let schemas = all_tool_schemas_with_env(|_| None);
        let write_file = find_schema(&schemas, "write_file").expect("write_file schema must exist");
        let func = write_file
            .get("function")
            .expect("write_file schema must include function block");
        let desc = func
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or_default();
        assert!(
            desc.contains("path")
                && desc.contains("content")
                && desc.contains("delete=true")
                && desc.contains("do not switch to bash"),
            "write_file description must spell out the path+content contract and discourage shell fallback: {desc}"
        );

        let params = func
            .get("parameters")
            .expect("write_file schema must include parameters");

        // Anthropic/Bedrock reject oneOf/allOf/anyOf at the top level of input_schema.
        // The write vs delete distinction is expressed via description prose and the
        // x-astra-per-action-required extension, not via composition keywords.
        assert!(
            params.get("oneOf").is_none(),
            "write_file parameters must not use top-level oneOf (Anthropic/Bedrock HTTP 400)"
        );
        assert!(
            params.get("allOf").is_none(),
            "write_file parameters must not use top-level allOf (Anthropic/Bedrock HTTP 400)"
        );
        assert!(
            params.get("anyOf").is_none(),
            "write_file parameters must not use top-level anyOf (Anthropic/Bedrock HTTP 400)"
        );

        // path must be the sole top-level required field.
        let required = params
            .get("required")
            .and_then(Value::as_array)
            .expect("write_file parameters must include a required array");
        assert!(
            required.iter().any(|v| v == "path"),
            "write_file must require path: {required:?}"
        );

        // Per-action required fields must be encoded in the vendor extension.
        let per_action = params.get("x-astra-per-action-required").expect(
            "write_file must use x-astra-per-action-required to encode per-mode requirements",
        );
        let write_req = per_action
            .get("write")
            .and_then(Value::as_array)
            .expect("x-astra-per-action-required must list fields required for write");
        assert!(
            write_req.iter().any(|v| v == "path") && write_req.iter().any(|v| v == "content"),
            "write action must require both path and content: {write_req:?}"
        );
        let delete_req = per_action
            .get("delete")
            .and_then(Value::as_array)
            .expect("x-astra-per-action-required must list fields required for delete");
        assert!(
            delete_req.iter().any(|v| v == "path"),
            "delete action must require path: {delete_req:?}"
        );
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
