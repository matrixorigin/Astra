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

/// Check whether a tool name has a corresponding schema in the built-in
/// registry. Used by [`super::tool_engine::ToolEngine::register_handler`]
/// to detect schema↔handler mismatches at registration time rather than
/// at runtime when the LLM calls an unimplemented or mis-specified tool.
pub fn schema_exists_for_tool(name: &str) -> bool {
    all_tool_schemas().iter().any(|schema| {
        schema
            .get("function")
            .and_then(|f| f.get("name"))
            .and_then(Value::as_str)
            == Some(name)
    })
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
    enforce_task_schema_unknown_field_contract(&mut schemas);
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
            "description": "Execute a PowerShell command. Use for Windows shell tasks, pwsh scripts, and cross-platform automation when PowerShell syntax is preferred over bash. PREFER dedicated tools (git, glob, grep, read_file, write_file, str_replace) over shell commands when they cover the operation.",
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

fn enforce_task_schema_unknown_field_contract(schemas: &mut [Value]) {
    if let Some(task) = schemas.iter_mut().find(|schema| {
        schema
            .get("function")
            .and_then(|function| function.get("name"))
            .and_then(Value::as_str)
            == Some("task")
    }) && let Some(parameters) = task
        .get_mut("function")
        .and_then(|function| function.get_mut("parameters"))
        .and_then(Value::as_object_mut)
    {
        parameters.insert("additionalProperties".to_string(), Value::Bool(false));
        if let Some(subtasks) = parameters
            .get_mut("properties")
            .and_then(Value::as_object_mut)
            .and_then(|properties| properties.get_mut("subtasks"))
            .and_then(Value::as_object_mut)
        {
            subtasks.insert(
                "maxItems".to_string(),
                Value::from(crate::task_mgmt::MAX_CREATE_SUBTASKS as u64),
            );
        }
    }
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
                    "additionalProperties": false,
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
                "description": "Execute a shell command. Use for builds, tests, installs, or actions with no dedicated tool. Identical commands are cached; set force=true to bypass.",
                "parameters": {
                    "type": "object",
                    "additionalProperties": false,
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
                "description": "Read file contents. Use exact fields only: path, start_line, end_line, outline. Do not use limit/offset/length. For the first N lines, use start_line=1 and end_line=N. For a range, start_line/end_line are inclusive line numbers; prefer start_line <= end_line. If a read-only reversed range is sent, the tool normalizes it and explains the resolved range. Set outline=true for function/class signatures only.",
                "parameters": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "path": {"type": "string", "description": "File path relative to project root"},
                        "start_line": {"type": "integer", "minimum": 1, "description": "First line to read (1-based). Prefer <= end_line when end_line is provided."},
                        "end_line": {"type": "integer", "minimum": 1, "description": "Last line to read (inclusive). Use this instead of limit/count."},
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
                "description": "Create, overwrite, or delete a file. For writes, provide `path` and `content`. Use this for new files, complete rewrites, or large changes (>4KB) — `str_replace` is a diff channel and should not be used for full-section replacements. WARNING: overwrites existing files silently — read first if you need to preserve content. For deletes, set `delete=true` and omit `content`. Retry `write_file` with corrected args; do not switch to bash or python just to write a file.",
                "parameters": {
                    "type": "object",
                    "additionalProperties": false,
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
                "description": "Targeted text replacement in files. Single mode: path+old_str+new_str. Batch mode: edits[]. Do not use aliases. For large changes (>4KB), use write_file.",
                "parameters": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "path": {"type": "string", "description": "File path relative to project root. Required for single mode and same-file batch mode; optional when every edits[] entry has its own path."},
                        "old_str": {"type": "string", "description": "String to replace. Required with new_str in single-edit mode; omit when using edits."},
                        "new_str": {"type": "string", "description": "Replacement text. Required with old_str in single-edit mode; omit when using edits."},
                        "edits": {
                            "type": "array",
                            "description": "Batch mode: array of {old_str, new_str, path?} edits. Top-level path applies to entries without path. If top-level path is omitted, every edit must include path. Mutually exclusive with top-level old_str/new_str.",
                            "items": {
                                "type": "object",
                                "additionalProperties": false,
                                "properties": {
                                    "path": {"type": "string", "description": "Optional file path for this edit; required when top-level path is omitted."},
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
                    "x-astra-per-action-required": {
                        "single": ["path", "old_str", "new_str"],
                        "batch_same_file": ["path", "edits"],
                        "batch_multi_file": ["edits[].path", "edits[].old_str", "edits[].new_str"]
                    }
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "rollback_file_edits",
                "description": "List or restore file edits recorded by write_file and str_replace. Use scope=current_turn to undo this turn's recorded file edits, scope=file with path to restore the latest recorded edit for one file, scope=turn with turn_index to restore a previous turn, or scope=list to inspect available file edit rollback entries.",
                "parameters": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "scope": {"type": "string", "enum": ["current_turn","turn","file","list"], "description": "Rollback scope. Defaults to current_turn; path implies file scope."},
                        "path": {"type": "string", "description": "File path for scope=file."},
                        "turn_index": {"type": "integer", "description": "Turn index for scope=turn."},
                        "file_after_sequence": {"type": "integer", "description": "Only restore file edits recorded after this journal sequence."},
                        "after_sequence": {"type": "integer", "description": "Alias for file_after_sequence."}
                    }
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "list_dir",
                "description": "List directory contents. Use to explore project structure or find files. For pattern-based file search (e.g. '**/*.rs'), use glob instead.",
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
                "description": "Find files matching a glob pattern. Supports pagination via offset/head_limit and sorting by mtime or path. Use for pattern-based file search (e.g. '**/*.rs', 'src/**/test_*'); use list_dir for interactive directory exploration instead.",
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
                "description": "Extract code symbols (functions, classes, structs, methods) from a file using AST parsing (tree-sitter). Supports Rust, Python, TypeScript/JavaScript, Go, Java, C/C++, Ruby. Returns structured symbol info with signatures, line numbers, and nesting. Set calls=true to show function calls within each symbol body (understand code flow without reading full source). Use kinds[] to filter by symbol type (fn, method, class, struct, trait, etc.), and pattern for regex name filtering. Use for: understanding file structure, finding specific symbols, generating documentation outlines.",
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
                "description": "Language Server Protocol operations. Set dry_run=false to apply writes (rename, format, code_action). WARNING: dry_run=false is a third write path alongside write_file and str_replace — it modifies files in-place via the LSP. Default true (preview-only).",
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
                "description": "Git operations: status, diff, log, show, blame, commit, stash, push, and worktree. Pass action as the first parameter.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "action": {
                            "type": "string",
                            "enum": ["status","diff","log","show","blame","file_history","log_search","contributors","commit","revert_commit","stash","checkout_file","worktree","push"],
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
                            "description": "Max entries to return. Used by: log (default 10, max 500 auto-throttled), file_history (default 10), log_search (default 200)."
                        },
                        "query": {
                            "type": "string",
                            "description": "Commit-message search query. Used by: log_search (required)."
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
                        },
                        "remote": {
                            "type": "string",
                            "description": "Remote name (e.g. 'origin'). Used by: push (required)."
                        },
                        "branch": {
                            "type": "string",
                            "description": "Target branch name. Used by: push (required)."
                        },
                        "force_with_lease": {
                            "type": "boolean",
                            "description": "Use --force-with-lease (safer than bare --force). Used by: push. Default false."
                        },
                        "set_upstream": {
                            "type": "boolean",
                            "description": "Set upstream tracking (-u). Used by: push. Default false."
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
                        "worktree": ["sub_action"],
                        "push": ["remote", "branch"]
                    }
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "github",
                "description": "GitHub operations. Per-action required fields: get_pr/ci_status→pr_number, get_issue→issue_number, create_issue→title. `repo` (owner/name or bare name) defaults to the first preferred repo or is inferred from git remote; pass explicitly when querying cross-repo or a repo not in the preferred list.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "action": {"type": "string", "enum": ["list_prs","get_pr","ci_status","repo_stats","list_issues","get_issue","create_issue"], "description": "GitHub operation"},
                        "repo": {"type": "string", "description": "owner/name or bare name (e.g. 'anthropics/reference-agent' or 'memoria'). Inferred from current git remote when omitted."},
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
                "description": "Memory operations: remember, recall, forget, update, reflect, and feedback. Pass action parameter.",
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
                "description": "Session lifecycle and history. Actions: config(path+value), sleep, history_page, history_search, history_around. Use dedicated tools for file rollback (`rollback_file_edits`), session-state rollback (`rollback_session_state`), context compression (`compress_context`), plan mode (`enter_plan_mode`/`exit_plan_mode`), and user questions (`ask_user`).",
                "parameters": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "action": {"type": "string", "enum": ["config","sleep","history_page","history_search","history_around"]},
                        "path": {"type": "string", "description": "Config path for action=config."},
                        "value": {"type": "string", "description": "Config value"},
                        "force": {"type": "boolean", "description": "Override config drift/mutation governor for action=config."},
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
                        "history_search": ["pattern"],
                        "history_around": ["item_seq"]
                    }
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "compress_context",
                "description": "Record a manual context-compression request for the current turn. Use when the session is carrying stale or bulky context and future turns should prefer a compacted history.",
                "parameters": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "reason": {"type": "string", "description": "Short reason for manual compression. Defaults to manual_request."}
                    }
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "rollback_session_state",
                "description": "List or restore server-side session-state mutations such as config overrides, task-state snapshots, and manual context-compression markers. This is for session state, not file contents; use rollback_file_edits for file rollback.",
                "parameters": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "scope": {"type": "string", "enum": ["current_turn", "turn", "list"], "description": "Rollback scope. Defaults to current_turn. Use list to inspect available rollback handles."},
                        "turn_index": {"type": "integer", "description": "Turn index when scope=turn."},
                        "session_state_after_sequence": {"type": "integer", "description": "Only restore entries recorded after this rollback-journal sequence."},
                        "after_sequence": {"type": "integer", "description": "Alias for session_state_after_sequence."}
                    }
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "mo_query",
                "description": "Run a MatrixOne SQL query. Destructive statements are blocked unless allow_destructive=true, and mutating queries capture a pre-state snapshot for rollback_database_snapshots.",
                "parameters": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "sql": {"type": "string", "description": "SQL to execute."},
                        "database": {"type": "string", "description": "Optional MatrixOne database name."},
                        "allow_destructive": {"type": "boolean", "description": "Explicitly allow destructive or mutating SQL when needed. Default false."}
                    },
                    "required": ["sql"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "rollback_database_snapshots",
                "description": "List or restore MatrixOne pre-state snapshots captured before mutating SQL. Use this for database rollback, not file or session-state rollback.",
                "parameters": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "scope": {"type": "string", "enum": ["current_turn", "turn", "snapshot", "list"], "description": "Rollback scope. Defaults to current_turn. Use list to inspect recorded snapshots."},
                        "turn_index": {"type": "integer", "description": "Turn index when scope=turn."},
                        "snapshot_id": {"type": "string", "description": "Snapshot identifier when scope=snapshot."},
                        "database": {"type": "string", "description": "Optional database name when restoring a specific snapshot."},
                        "database_after_sequence": {"type": "integer", "description": "Only restore database snapshot entries recorded after this journal sequence."},
                        "after_sequence": {"type": "integer", "description": "Alias for database_after_sequence."}
                    }
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "agent",
                "description": "Actions: spawn needs description+prompt (not task/type/agent_id; foreground; no background arg); get_result needs returned agent_id; run_chain needs steps.\n\n\
         Multi-agent operations. Actions: spawn, get_result, run_chain, send_message.\n\n\
         ## Required fields per action\n\
         - `spawn`: REQUIRES `action`, `description`, `prompt`. (Optional: `agent_type`, `model`, `max_turns`, `complexity`, `isolated`, `allowed_tools`, `name`.)\n\
         - `get_result`: REQUIRES `action`, `agent_id`.\n\
         - `run_chain`: REQUIRES `action`, `steps`.\n\
         - `send_message`: REQUIRES `action`, `to`, `message`.\n\n\
         For `spawn`, pass both non-empty fields: `description` (short UI summary) and `prompt` (full child brief). Do NOT pass a top-level `task` field. Do NOT pass `type`; use `agent_type`. Do NOT pass `inherit_context`. `agent_id` is ONLY for `get_result`; never prefill it on `spawn`. Astra generates that runtime id for you. Later `get_result` calls must reuse the exact returned `agent_id`. If you need a mailbox label, use `name`, but `name` is not valid for `get_result`.\n\n\
         ## Spawn example\n\
         `agent(action='spawn', description='Audit auth flow', prompt='Read src/auth/* and report any token-handling bugs. Focus on session expiry and refresh logic. Return findings as a numbered list.', agent_type='general-purpose')`\n\n\
         ## Execution mode\n\
         `spawn` is foreground by contract: it blocks until the sub-agent's final result is ready, and the sub-agent's tool calls stream back inline. Backgrounding is user-controlled from the UI with Ctrl+B while the live agent is running; do not pass a background flag in tool arguments.\n\n\
         ## Parallel sub-agent fan-out\n\
         Use `agent_fanout(action='start', target_count=N, slots=[...])` to run a fixed-size parallel group atomically. It waits for slot results and returns them in the same tool call unless the user backgrounds the live run with Ctrl+B. Slots may include `id` as a caller-facing label; runtime-generated `agent_id` values come back in the result. Do not simulate fan-out with an `agents:[...]` payload on `agent`.\n\
         For plan lifecycle, call `enter_plan_mode` / `exit_plan_mode` directly. Do NOT wrap them inside `agent(action='run_chain', ...)`.\n\
         Do NOT pass an `agents:[...]` payload, do NOT pass a top-level `task` field, and do NOT wrap spawn arguments under a `spawn` field. `agent` launches one child; `agent_fanout` launches a fixed parallel group.

         ## agent vs shell work vs task
         - `agent(spawn)` + `agent(get_result)`: one synchronous or background sub-agent you plan to collect results from.
         - `agent_fanout`: fixed-size parallel sub-agent groups with target-count accounting.
         - Shell commands/processes are separate execution tools; do not represent them as sub-agents.
         - `task`: session checklist / progress tracking — NOT an executor. Tasks track work; tools run it.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "action": {"type": "string", "enum": ["spawn","get_result","run_chain","send_message"]},
                        "steps": {"type": "array", "description": "REQUIRED for action='run_chain'. Sequence of chain steps to execute."},
                        "description": {"type": "string", "description": "Spawn summary shown in the UI Task card. Short, specific, non-empty."},
                        "prompt": {"type": "string", "description": "Full child task brief for spawn. Non-empty and required with description."},
                        "agent_type": {"type": "string", "enum": ["explore","code-review","task","general-purpose"], "description": "Sub-agent persona (spawn). Default: general-purpose."},
                        "model": {"type": "string", "description": "Model override (spawn). Default: parent's model."},
                        "name": {"type": "string", "description": "Addressable mailbox name (spawn). Optional; auto-generated if omitted. Not the runtime agent_id used by get_result."},
                        "max_turns": {"type": "integer", "description": "Requested max turns (spawn). Runtime may raise too-small values for deep/code-review fanouts; omit it and use `complexity` when unsure."},
                        "complexity": {"type": "string", "enum": ["light","normal","deep"], "description": "Task-complexity hint scaling the default budget. `light`≈10 turns, `normal`=agent default, `deep`=2× default. Use `deep` for review/refactor/multi-file tasks that routinely exhaust the default."},
                        "isolated": {"type": "boolean", "description": "Use isolated worktree (spawn)"},
                        "allowed_tools": {"type": "array", "items": {"type": "string"}, "description": "Tool allowlist (spawn)"},
                        "agent_id": {"type": "string", "description": "ONLY for action='get_result'. Must be the exact runtime-generated agent_id returned by a prior spawn, not the optional spawn name. Never prefill this on spawn."},
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
                "name": "agent_fanout",
                "description": "Atomic parallel sub-agent fan-out: start needs target_count and exactly target_count slots; each slot needs description+prompt; optional id; no brief/agents/background.\n\n\
         Actions: start, get_results, stop_slot.\n\n\
         - `start`: REQUIRES `action`, `target_count`, `slots`. `slots` length must equal `target_count`; every slot requires `description` and `prompt`. Optional per-slot `id` is a stable caller label returned in start/results/fanout projections. Foreground mode waits for all accepted slots and returns `results`; backgrounding is user-controlled with Ctrl+B, not a tool argument.\n\
         - `get_results`: REQUIRES `action`, `group_id`. Blocks until accepted slots finish, then returns every slot result and the fanout summary.\n\
         - `stop_slot`: REQUIRES `action`, `group_id`, `slot_index`. Cancels one running slot in the group.\n\n\
         Canonical start shape for two children: `agent_fanout(action='start', target_count=2, slots=[{id:'api', description:'Review API', prompt:'Full child task prompt for API'}, {id:'ui', description:'Review UI', prompt:'Full child task prompt for UI'}], defaults={agent_type:'code-review'})`.\n\
         Use this instead of `agent` when the user asks for multiple reviewers, parallel exploration, or N independent sub-agents. Do not pass an `agents:[...]` payload to `agent`. Do not put top-level `brief`, `agents`, or `run_in_background` on `agent_fanout`; put full work instructions in each `slots[i].prompt`. Do not put `agent_id` inside slots; use `id` for the caller-facing slot label.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "action": {"type": "string", "enum": ["start","get_results","stop_slot"]},
                        "group_id": {"type": "string", "description": "Fanout group id. Optional on start; required for get_results and stop_slot."},
                        "title": {"type": "string", "description": "Optional short label for the group."},
                        "target_count": {"type": "integer", "minimum": 1, "description": "REQUIRED for start. Fixed number of slots to launch; must equal slots.length."},
                        "slots": {
                            "type": "array",
                            "description": "REQUIRED for start. One entry per parallel child.",
                            "items": {
                                "type": "object",
                                "additionalProperties": false,
                                "properties": {
                                    "id": {"type": "string", "description": "Optional stable caller-facing label for this slot. Returned in start/results/fanout projections. Not the runtime agent_id."},
                                    "description": {"type": "string", "description": "Short UI summary for this slot."},
                                    "prompt": {"type": "string", "description": "Full child task brief for this slot."},
                                    "agent_type": {"type": "string", "enum": ["explore","code-review","task","general-purpose"]},
                                    "model": {"type": "string"},
                                    "max_turns": {"type": "integer"},
                                    "max_output_tokens": {"type": "integer"},
                                    "complexity": {"type": "string", "enum": ["light","normal","deep"]},
                                    "isolated": {"type": "boolean"},
                                    "allowed_tools": {"type": "array", "items": {"type": "string"}}
                                },
                                "required": ["description", "prompt"]
                            }
                        },
                        "defaults": {
                            "type": "object",
                            "description": "Shared runtime configuration inherited by every slot. Slot-level overrides take precedence.",
                            "additionalProperties": false,
                            "properties": {
                                "agent_type": {"type": "string", "enum": ["explore","code-review","task","general-purpose"]},
                                "model": {"type": "string"},
                                "max_turns": {"type": "integer"},
                                "max_output_tokens": {"type": "integer"},
                                "complexity": {"type": "string", "enum": ["light","normal","deep"]},
                                "isolated": {"type": "boolean"},
                                "allowed_tools": {"type": "array", "items": {"type": "string"}}
                            }
                        },
                        "slot_index": {"type": "integer", "description": "REQUIRED for stop_slot. Zero-based slot index."}
                    },
                    "required": ["action"],
                    "additionalProperties": false,
                    "x-astra-per-action-required": {
                        "start": ["target_count", "slots"],
                        "get_results": ["group_id"],
                        "stop_slot": ["group_id", "slot_index"]
                    }
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "introspect",
                "description": "Query runtime state. Subtopics: `session` (default: token pressure, cache hit rate, tool health, alerts, working memory, plan/task/session lifecycle context including restore/resume state and last lifecycle event when available), `cache` (cache-regression diagnosis), `recent` (recent LLM-round summaries), `volatile` (runtime nudges/coaching queued for next turn), `stall` (loop-guard state), `all` (session + recent + volatile + stall). Use `detail: full` for deep diagnosis (stall forensics, context pressure, performance); use `detail: summary` (default) for quick health checks.",
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
                "name": "get_agent_info",
                "description": "Return the current Astra agent identity and capability summary. Use dimension='capability' to inspect which tools are actually available under the current workspace, executor, runtime, and policy binding.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "dimension": {
                            "type": "string",
                            "enum": ["identity", "capability", "all"],
                            "description": "Information slice to return. Defaults to all."
                        }
                    },
                    "additionalProperties": false
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "tool_search",
                "description":
                    "Search deferred tools. Keywords list candidates. `select:NAME[,NAME]` \
                     returns compact callable shape and queues schemas for the next request. \
                     `detail:NAME` expands full docs only when needed.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description":
                                "Keyword query, `select:NAME` / `select:NAME1,NAME2`, or `detail:NAME`."
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
                "description": "Send a notification to the user. Use for proactive updates (background task done, blocker found, unsolicited insight). Gateways route based on notification_type: 'normal' = in-chat reply, 'proactive' = push notification. CLI mode: both render as text. Example: notify(message='Build completed successfully', notification_type='proactive').",
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
                "description": "Ask the user structured questions when a decision is needed. Supports 1-6 questions, headers, options, multi_select, and allow_freeform. Use for clarifications.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "context": {"type": "string", "description": "Brief context shown above the questionnaire (dimmed)"},
                        "questions": {
                            "type": "array",
                            "description": "1-6 questions to present in the ask_user questionnaire.",
                            "minItems": 1,
                            "maxItems": 6,
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
        json!({
            "type": "function",
            "function": {
                "name": "task",
                "description": "Checklist for multi-step work. Use subtasks for 3+ outcomes, files, or phases. Update progress with new_status; list with status_filter.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "action": {"type": "string", "enum": ["create","update","list","get","stop","list_user","adopt","archive"], "description": "Operation."},
                        "source_session_id": {"type": "string", "description": "(adopt) Source session id"},
                        "older_than_days": {"type": "integer", "description": "(archive bulk; omit task_id) Archive completed older than N days. Default 30."},
                        "user_status": {"type": "string", "enum": ["active","pending","in_progress","paused","completed","failed","cancelled","archived","all"], "description": "(list_user) Cross-session. Default active = open work: pending + in_progress + paused."},
                        "title": {"type": "string", "description": "(create/update) Title."},
                        "description": {"type": "string", "description": "(create/update) Done."},
                        "task_id": {"type": "string", "description": "(update/get/stop/adopt/archive) Task id. Single-task archive stays in current session."},
                        "new_status": {"type": "string", "enum": ["pending","in_progress","paused","completed","failed","cancelled","deleted"], "description": "(update only) New task/subtask status. Do not send `status`. Only one parent task may be in_progress; `paused` frees that slot; `deleted` removes the task."},
                        "status_filter": {"type": "string", "enum": ["pending","in_progress","paused","completed","failed","cancelled","archived","all","active"], "description": "(list only) Use `status_filter: \"all\"` to list all tasks; do not send an `all` boolean. `active` = pending + in_progress + paused."},
                        "subtask_id": {"type": "string", "description": "(update) Subtask id; use with task_id + new_status, optional reason."},
                        "active_form": {"type": "string", "description": "(create/update) Spinner text while in_progress."},
                        "owner": {"type": "string", "description": "(create/update) Owner."},
                        "metadata": {"type": "object", "description": "(create/update) Key-value pairs; null deletes a key on update."},
                        "add_blocks": {"type": "array", "items": {"type": "string"}, "description": "(create/update) Task ids this task blocks; edge is symmetric. Blocked tasks wait for completed blockers."},
                        "add_blocked_by": {"type": "array", "items": {"type": "string"}, "description": "(create/update) Task ids blocking this task. It cannot start until every blocker is completed or removed."},
                        "remove_blocks": {"type": "array", "items": {"type": "string"}, "description": "(update only; never with create) Remove symmetric blocks edges."},
                        "remove_blocked_by": {"type": "array", "items": {"type": "string"}, "description": "(update only; never with create) Remove symmetric blocked_by edges."},
                        "subtasks": {
                            "type": "array",
                            "description": "(create only) Optional subtasks. Do not send on update; use subtask_id + new_status to update existing subtask progress.",
                            "items": {
                                "type": "object",
                                "additionalProperties": false,
                                "properties": {
                                    "id": {"type": "string"},
                                    "title": {"type": "string"},
                                    "description": {"type": "string"},
                                    "depends_on": {"type": "array", "items": {"type": "string"}, "description": "Sibling ids completed before this subtask starts or completes."},
                                    "owner": {"type": "string"}
                                },
                                "required": ["id", "title"]
                            }
                        },
                        "reason": {"type": "string", "description": "(update/stop/archive) Reason. With subtask_id, stores a subtask status note; failed parent fills error_message if omitted."},
                        "error_message": {"type": "string", "description": "(update) Failure/cancel reason to include when setting new_status='failed' or new_status='cancelled'."}
                    },
                    "required": ["action"],
                    "x-astra-per-action-required": {
                        "create": ["title"],
                        "update": ["task_id"],
                        "get": ["task_id"],
                        "stop": ["task_id"],
                        "adopt": ["source_session_id", "task_id"]
                    }
                }
            }
        }),
        // ── background task control ─────────────────────────────────
        // Typed control surface for background tasks. Starting shell work stays
        // on Bash / Ctrl+B and local agents stay on agent(); control actions
        // use explicit tools rather than a generic action union.
        json!({
            "type": "function",
            "function": {
                "name": "task_output",
                "description": "Read output for a specific typed background task. Use this after a background task notification or task_list entry. Returns explicit task kind, status, byte offsets, total bytes, and the requested output chunk when available. Requires the exact task_id so the model and UI refer to the same background task.",
                "parameters": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "task_id": {
                            "type": "string",
                            "description": "Background task id, such as bg-shell-3 or a local agent id."
                        },
                        "block": {
                            "type": "boolean",
                            "description": "Wait for new output or terminal status before returning. Default false; set true only when the user explicitly asks to wait."
                        },
                        "offset": {
                            "type": "integer",
                            "minimum": 0,
                            "description": "Resume reading from this byte offset. Default 0."
                        },
                        "max_bytes": {
                            "type": "integer",
                            "minimum": 1,
                            "description": "Maximum bytes to return from the current offset. Default 8192, max 65536."
                        },
                        "timeout_ms": {
                            "type": "integer",
                            "description": "Max ms to wait when block=true, and max registry response wait when block=false. Default 30000, max 300000."
                        }
                    },
                    "required": ["task_id"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "task_stop",
                "description": "Stop a running typed background task by id. Use for stuck shell tasks, waiting-for-input tasks, local agents, or tasks the user explicitly wants cancelled. Requires an exact task_id; does not stop the most recent task implicitly.",
                "parameters": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "task_id": {
                            "type": "string",
                            "description": "Background task id to stop, such as bg-shell-3 or a local agent id."
                        }
                    },
                    "required": ["task_id"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "task_list",
                "description": "List known typed background tasks for this session with kind, status, and ids. Use when you need to discover which background task to inspect or stop.",
                "parameters": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "include_terminal": {
                            "type": "boolean",
                            "description": "Include recently completed, failed, or killed tasks. Default true."
                        }
                    }
                }
            }
        }),
        // ── enter_plan_mode ─────────────────────────────────────────
        // Top-level sentinel tool that flips the session into plan
        // mode. Promoted from the buried `session.enter_plan` action
        // in 2026-05 because the model rarely picked the sub-action
        // — the reference agent's dedicated `EnterPlanMode` tool is the
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
        5. Call `exit_plan_mode(plan='<markdown>')` to submit the plan for user approval. Approval is produced by the UI/control plane, not by model-supplied tool arguments.\n\
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
        // success, and mirrors the approved plan into the session task
        // board so the next turn can execute step-by-step.
        json!({
            "type": "function",
            "function": {
                "name": "exit_plan_mode",
                "description": "Submit the plan for user approval. The `plan` argument is a markdown string (numbered list, nested bullets ok) that the user reads and either approves or rejects in the trusted UI. The model cannot approve its own plan; approval unlocks writes only after the UI/control plane returns the user's decision. After trusted approval, the approved work appears in the session task board.\n\
        \n\
        ## Plan structure (what makes a good plan)\n\
        - Numbered list of concrete, executable leaf steps — each step maps to ONE artifact, API surface, or validation target.\n\
        - Each step includes: what files to touch, what to change, and the acceptance criteria.\n\
        - Avoid umbrella phases like \"build the system\" — split into scaffold → implement → test → verify.\n\
        - Prefer 3-7 steps for most work; >10 steps signals over-decomposition.\n\
        \n\
        ## Important\n\
        - Do NOT call this tool to ask 'is the plan ready?' — that's exactly what THIS tool does. It inherently requests approval.\n\
        - Pass the FULL plan as a single markdown string in `plan`. The user sees this verbatim.\n\
        - Only call this when the plan is concrete and unambiguous. If you have unresolved decisions, use `ask_user` first.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "plan": {
                            "type": "string",
                            "description": "The plan markdown to present for approval. Numbered list of steps; nested bullets ok. The user reads this verbatim."
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

    // ── agent tool: foreground default + Ctrl+B backgrounding contract ─────

    #[test]
    fn agent_schema_does_not_expose_model_background_parameter() {
        let schemas = all_tool_schemas_with_env(|_| None);
        let agent = find_schema(&schemas, "agent").expect("agent schema must exist");
        let props = agent
            .get("function")
            .and_then(|f| f.get("parameters"))
            .and_then(|p| p.get("properties"))
            .expect("agent must expose parameters.properties");
        assert!(
            props.get("run_in_background").is_none(),
            "backgrounding must be user-controlled with Ctrl+B, not model-controlled by schema"
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
            desc.contains("Ctrl+B"),
            "agent description must point backgrounding at the user-controlled Ctrl+B path"
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
            desc.contains("agent_fanout"),
            "agent description must point parallel fan-out at the atomic tool"
        );
        assert!(
            !desc.contains("ONLY way to fan out"),
            "agent description must not keep the old N-spawn fanout contract"
        );
        assert!(
            desc.contains("exit_plan_mode") && desc.contains("run_chain"),
            "agent description should steer plan lifecycle away from run_chain"
        );
    }

    #[test]
    fn agent_fanout_schema_exposes_atomic_group_contract() {
        let schemas = all_tool_schemas_with_env(|_| None);
        let fanout = find_schema(&schemas, "agent_fanout").expect("agent_fanout schema must exist");
        let desc = fanout
            .get("function")
            .and_then(|f| f.get("description"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        let params = &fanout["function"]["parameters"];

        assert!(desc.contains("Atomic parallel sub-agent fan-out"));
        assert!(desc.contains("`id`"));
        assert!(desc.contains("Ctrl+B"));
        assert!(desc.contains("Foreground mode") || desc.contains("returns `results`"));
        assert!(desc.contains("top-level `brief`"));
        assert_eq!(params["additionalProperties"], false);
        assert_eq!(
            params["properties"]["action"]["enum"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>(),
            vec!["start", "get_results", "stop_slot"]
        );
        assert_eq!(
            params["x-astra-per-action-required"]["start"],
            json!(["target_count", "slots"])
        );
        assert_eq!(
            params["properties"]["slots"]["items"]["required"],
            json!(["description", "prompt"])
        );
        assert!(params["properties"].get("run_in_background").is_none());
        let slot_props = &params["properties"]["slots"]["items"]["properties"];
        assert!(
            slot_props.get("id").is_some(),
            "fanout slots must expose the canonical caller-facing identity field"
        );
        assert!(slot_props.get("slot_id").is_none());
        assert!(
            slot_props.get("name").is_none(),
            "fanout slots should not expose spawn mailbox names as slot identity"
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
            desc.contains("`agent_id` is ONLY for `get_result`")
                || desc.contains("never prefill it on `spawn`"),
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
        assert!(
            params["properties"]["agent_id"]["description"]
                .as_str()
                .unwrap_or("")
                .contains("Never prefill this on spawn"),
            "agent_id field must explicitly forbid spawn-time prefill"
        );
    }

    #[test]
    fn typed_background_task_schemas_replace_job_public_contract() {
        let schemas = all_tool_schemas_with_env(|_| None);
        assert!(
            find_schema(&schemas, "job").is_none()
                && find_schema(&schemas, "task_output").is_some()
                && find_schema(&schemas, "task_stop").is_some()
                && find_schema(&schemas, "task_list").is_some(),
            "model-facing schema must expose typed background task tools, not generic job"
        );
        assert!(
            find_schema(&schemas, "agent_job").is_none()
                && find_schema(&schemas, "task_output").is_some(),
            "agent_job must not remain in the model-facing schema; use agent background lifecycle separately"
        );
        let output_desc = find_schema(&schemas, "task_output")
            .and_then(|schema| {
                schema
                    .get("function")
                    .and_then(|f| f.get("description"))
                    .and_then(Value::as_str)
            })
            .unwrap_or_default();
        assert!(
            output_desc.contains("background task") && !output_desc.contains("job(action"),
            "task_output description must teach typed background task vocabulary"
        );
        let output_block_desc = find_schema(&schemas, "task_output")
            .and_then(|schema| {
                schema
                    .get("function")
                    .and_then(|f| f.get("parameters"))
                    .and_then(|p| p.get("properties"))
                    .and_then(|p| p.get("block"))
                    .and_then(|p| p.get("description"))
                    .and_then(Value::as_str)
            })
            .unwrap_or_default();
        assert!(
            output_block_desc.contains("Default false"),
            "task_output must default to snapshot reads unless the user asks to wait"
        );
        assert!(
            find_schema(&schemas, "agent_job").is_none(),
            "agent_job must not remain in the model-facing schema"
        );
    }

    #[test]
    fn task_schema_keeps_compact_multi_step_contract() {
        let schemas = all_tool_schemas_with_env(|_| None);
        let task = find_schema(&schemas, "task").expect("task schema must exist");
        let desc = task
            .get("function")
            .and_then(|f| f.get("description"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        assert!(
            desc.len() <= 140,
            "task description should stay compact in the always-load prefix: {desc}"
        );
        assert!(
            desc.contains("3+ outcomes") && desc.contains("subtasks"),
            "task description must name the explicit '3+ outcomes/steps' threshold so the model has \
             a hard trigger, not a fuzzy heuristic"
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
    fn always_load_high_frequency_descriptions_stay_compact() {
        let schemas = all_tool_schemas_with_env(|_| None);
        for (name, max_len) in [
            ("bash", 180usize),
            ("str_replace", 180),
            ("git", 140),
            ("memory", 120),
            ("ask_user", 180),
            ("task", 140),
            ("tool_search", 240),
        ] {
            let schema = find_schema(&schemas, name).expect("schema must exist");
            let desc = schema["function"]["description"].as_str().unwrap_or("");
            assert!(
                desc.len() <= max_len,
                "{name} description regressed to {} chars; max {max_len}: {desc}",
                desc.len()
            );
        }
    }

    #[test]
    fn task_schema_discourages_single_umbrella_task() {
        let schemas = all_tool_schemas_with_env(|_| None);
        let task = find_schema(&schemas, "task").expect("task schema must exist");
        let desc = task["function"]["description"].as_str().unwrap();

        assert!(desc.contains("multi-step work"));
        assert!(desc.contains("3+ outcomes"));
    }

    #[test]
    fn task_schema_exposes_lifecycle_progress_and_dependencies() {
        let schemas = all_tool_schemas_with_env(|_| None);
        let task = find_schema(&schemas, "task").expect("task schema must exist");
        let properties = &task["function"]["parameters"]["properties"];
        assert_eq!(
            task["function"]["parameters"]["additionalProperties"], false,
            "task schema should reject unknown top-level fields"
        );

        for field in [
            "active_form",
            "add_blocks",
            "add_blocked_by",
            "error_message",
        ] {
            assert!(
                properties.get(field).is_some(),
                "task schema must expose {field} to the model"
            );
        }
        let error_message_desc = properties["error_message"]["description"]
            .as_str()
            .unwrap_or_default();
        assert!(
            error_message_desc.contains("new_status='failed'"),
            "error_message should be explicitly tied to failed task updates: {error_message_desc}"
        );
        assert!(
            error_message_desc.contains("new_status='cancelled'"),
            "error_message should also support cancelled task updates: {error_message_desc}"
        );
        assert!(
            properties["active_form"]["description"]
                .as_str()
                .unwrap_or_default()
                .contains("Spinner text"),
            "active_form should stay product-facing spinner guidance"
        );
        let action_enum = properties["action"]["enum"]
            .as_array()
            .expect("task action enum");
        assert!(
            action_enum.iter().any(|v| v.as_str() == Some("archive")),
            "task schema should expose archive as a structured action"
        );
        assert!(
            properties["older_than_days"]["description"]
                .as_str()
                .unwrap_or_default()
                .contains("Archive completed"),
            "archive bulk criteria should live on older_than_days, not in the always-load description"
        );
        let subtask_id_desc = properties["subtask_id"]["description"]
            .as_str()
            .unwrap_or_default();
        assert!(
            subtask_id_desc.contains("optional reason"),
            "subtask_id should explain the narrow subtask-update shape: {subtask_id_desc}"
        );
        let reason_desc = properties["reason"]["description"]
            .as_str()
            .unwrap_or_default();
        assert!(
            reason_desc.contains("With subtask_id") && reason_desc.contains("subtask status note"),
            "reason should be advertised for explained subtask updates: {reason_desc}"
        );
        assert!(
            properties["status_filter"]
                .as_object()
                .and_then(|_| properties["status_filter"]["enum"].as_array())
                .is_some_and(|values| values.iter().any(|v| v.as_str() == Some("archived"))),
            "task schema should let the model query archived tasks explicitly"
        );
        assert!(
            properties["status_filter"]
                .as_object()
                .and_then(|_| properties["status_filter"]["enum"].as_array())
                .is_some_and(|values| values.iter().any(|v| v.as_str() == Some("cancelled"))),
            "task schema should let the model query cancelled tasks explicitly"
        );
        assert!(
            properties["status_filter"]
                .as_object()
                .and_then(|_| properties["status_filter"]["enum"].as_array())
                .is_some_and(|values| values.iter().any(|v| v.as_str() == Some("paused"))),
            "task schema should let the model query auto-paused tasks explicitly"
        );
        let status_filter_desc = properties["status_filter"]["description"]
            .as_str()
            .unwrap_or_default();
        assert!(
            status_filter_desc.contains("active")
                && status_filter_desc.contains("pending + in_progress + paused"),
            "task schema should explain task.list active includes paused open work: {status_filter_desc}"
        );
        assert!(
            status_filter_desc.contains("status_filter")
                && status_filter_desc.contains("\"all\"")
                && status_filter_desc.contains("do not send an `all` boolean"),
            "task schema should steer list-all away from an unsupported all field: {status_filter_desc}"
        );
        assert!(
            properties.get("status").is_none(),
            "task schema must not expose the old status field; use new_status/status_filter"
        );
        assert!(
            properties.get("all").is_none(),
            "task schema must not expose an all boolean; use status_filter='all'"
        );
        assert!(
            properties["new_status"]
                .as_object()
                .and_then(|_| properties["new_status"]["enum"].as_array())
                .is_some_and(|values| values.iter().any(|v| v.as_str() == Some("paused"))),
            "task schema should let the model intentionally pause/resume stale work"
        );
        let new_status_desc = properties["new_status"]["description"]
            .as_str()
            .unwrap_or_default();
        assert!(
            new_status_desc.contains("Only one parent task may be in_progress"),
            "new_status should teach the single in_progress task invariant: {new_status_desc}"
        );
        assert!(
            new_status_desc.contains("Do not send `status`"),
            "new_status should explicitly reject the old status alias: {new_status_desc}"
        );
        let add_blocked_by_desc = properties["add_blocked_by"]["description"]
            .as_str()
            .unwrap_or_default();
        assert!(
            add_blocked_by_desc.contains("completed or removed"),
            "blocked_by should explain blockers must resolve before start: {add_blocked_by_desc}"
        );
        assert!(
            add_blocked_by_desc.contains("create/update"),
            "blocked_by should expose create-time dependencies: {add_blocked_by_desc}"
        );
        let add_blocks_desc = properties["add_blocks"]["description"]
            .as_str()
            .unwrap_or_default();
        assert!(
            add_blocks_desc.contains("edge is symmetric"),
            "blocks should explain task dependency edges are symmetric: {add_blocks_desc}"
        );
        assert!(
            add_blocks_desc.contains("create/update"),
            "blocks should expose create-time dependencies: {add_blocks_desc}"
        );
        let depends_on_desc =
            properties["subtasks"]["items"]["properties"]["depends_on"]["description"]
                .as_str()
                .unwrap_or_default();
        assert!(
            depends_on_desc.contains("before this subtask starts or completes"),
            "subtask depends_on should explain execution order constraints: {depends_on_desc}"
        );
        let subtask_item = &properties["subtasks"]["items"];
        let subtasks_desc = properties["subtasks"]["description"]
            .as_str()
            .unwrap_or_default();
        assert!(
            subtasks_desc.contains("create only")
                && subtasks_desc.contains("subtask_id + new_status"),
            "subtasks property should prevent task.update(subtasks) misuse: {subtasks_desc}"
        );
        assert_eq!(
            properties["subtasks"]["maxItems"].as_u64(),
            Some(crate::task_mgmt::MAX_CREATE_SUBTASKS as u64),
            "task schema should expose the same subtask fan-out limit as TaskManager"
        );
        assert_eq!(
            subtask_item["additionalProperties"], false,
            "subtask schema should reject unknown fields"
        );
        assert!(
            subtask_item["properties"].get("owner").is_some(),
            "subtask schema should expose the supported owner field"
        );
        assert!(
            properties["user_status"]
                .as_object()
                .and_then(|_| properties["user_status"]["enum"].as_array())
                .is_some_and(|values| values.iter().any(|v| v.as_str() == Some("cancelled"))),
            "task schema should let the model query cancelled cross-session tasks explicitly"
        );
        assert!(
            properties["user_status"]
                .as_object()
                .and_then(|_| properties["user_status"]["enum"].as_array())
                .is_some_and(|values| values.iter().any(|v| v.as_str() == Some("paused"))),
            "task schema should let the model query paused cross-session tasks explicitly"
        );
        let user_status_desc = properties["user_status"]["description"]
            .as_str()
            .unwrap_or_default();
        assert!(
            user_status_desc.contains("Default active")
                && user_status_desc.contains("pending + in_progress + paused"),
            "task schema should explain list_user active includes paused open work: {user_status_desc}"
        );
        let per_action_required = task["function"]["parameters"]["x-astra-per-action-required"]
            .as_object()
            .expect("task schema must expose per-action required fields");
        let adopt_required = per_action_required
            .get("adopt")
            .and_then(|value| value.as_array())
            .expect("adopt should list required fields");
        assert!(
            adopt_required
                .iter()
                .any(|value| value.as_str() == Some("source_session_id"))
                && adopt_required
                    .iter()
                    .any(|value| value.as_str() == Some("task_id")),
            "adopt requires both source_session_id and task_id: {adopt_required:?}"
        );
    }

    #[test]
    fn plan_schema_uses_semantic_guidance_not_lexical_triggers() {
        let schemas = all_tool_schemas_with_env(|_| None);
        let enter =
            find_schema(&schemas, "enter_plan_mode").expect("enter_plan_mode schema must exist");
        let plan_desc = enter["function"]["description"].as_str().unwrap();

        assert!(
            plan_desc.contains("## When to Use") && plan_desc.contains("## When NOT to Use"),
            "plan tool UX guidance should remain semantic and example-driven: {plan_desc}"
        );
        assert!(
            !plan_desc.contains("ACTIVATION RULE"),
            "plan schema must not expose matcher-style activation rules: {plan_desc}"
        );
        assert!(
            !plan_desc.contains("conjunctions like"),
            "plan schema must not teach lexical trigger matching: {plan_desc}"
        );
        assert!(
            !plan_desc.contains("\"what's the best way to\"")
                && !plan_desc.contains("\"redesign\""),
            "plan schema must not encode phrase-list triggers: {plan_desc}"
        );
    }

    #[test]
    fn exit_plan_mode_schema_points_to_task_board_not_legacy_plan_todos() {
        let schemas = all_tool_schemas_with_env(|_| None);
        let exit =
            find_schema(&schemas, "exit_plan_mode").expect("exit_plan_mode schema must exist");
        let desc = exit["function"]["description"].as_str().unwrap();
        let properties = exit["function"]["parameters"]["properties"]
            .as_object()
            .expect("exit_plan_mode properties must be an object");

        assert!(
            desc.contains("session task board"),
            "approved plans should surface through the user-visible task board: {desc}"
        );
        assert!(
            desc.contains("model cannot approve its own plan"),
            "schema must make user approval ownership explicit: {desc}"
        );
        assert!(
            !properties.contains_key("approved"),
            "model-facing exit_plan_mode schema must not expose an approval parameter"
        );
        assert!(
            !desc.contains("session_plan_todos"),
            "schema must not expose the old internal plan todo queue: {desc}"
        );
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
    fn self_mod_session_state_top_level_schemas_exist() {
        let schemas = all_tool_schemas_with_env(|_| None);
        for name in ["compress_context", "rollback_session_state"] {
            find_schema(&schemas, name)
                .expect("top-level schema must exist for ToolEngine routing");
        }

        for retired in ["prioritize", "deprioritize"].map(|prefix| format!("{prefix}_tool")) {
            assert!(
                find_schema(&schemas, &retired).is_none(),
                "{retired} must not remain in the tool schema surface"
            );
        }
    }

    #[test]
    fn matrixone_top_level_schemas_exist() {
        let schemas = all_tool_schemas_with_env(|_| None);
        assert!(
            find_schema(&schemas, "mo").is_none(),
            "MatrixOne must expose one public query shape; do not keep the old aggregate mo schema"
        );
        for name in ["mo_query", "rollback_database_snapshots"] {
            find_schema(&schemas, name)
                .expect("top-level schema must exist for ToolEngine routing");
        }

        let mo_query = find_schema(&schemas, "mo_query").expect("mo_query schema");
        let required = mo_query["function"]["parameters"]["required"]
            .as_array()
            .expect("mo_query should declare required fields");
        assert!(
            required.iter().any(|value| value.as_str() == Some("sql")),
            "mo_query schema must require sql: {mo_query:?}"
        );
        let rollback =
            find_schema(&schemas, "rollback_database_snapshots").expect("rollback schema");
        let scopes = rollback["function"]["parameters"]["properties"]["scope"]["enum"]
            .as_array()
            .expect("rollback scope should have enum values")
            .iter()
            .filter_map(Value::as_str)
            .collect::<std::collections::HashSet<_>>();
        assert!(scopes.contains("snapshot"));
        assert!(scopes.contains("list"));
    }

    #[test]
    fn session_schema_excludes_retired_session_state_actions() {
        let schemas = all_tool_schemas_with_env(|_| None);
        let session = find_schema(&schemas, "session").expect("session schema");
        let actions = session["function"]["parameters"]["properties"]["action"]["enum"]
            .as_array()
            .expect("session action enum")
            .iter()
            .filter_map(Value::as_str)
            .collect::<std::collections::HashSet<_>>();

        for retired in [
            "prioritize",
            "deprioritize",
            "compact",
            "set_goal",
            "ask_user",
        ] {
            assert!(
                !actions.contains(retired),
                "session must not expose retired action {retired}; use the dedicated tool"
            );
        }
        for current in [
            "config",
            "sleep",
            "history_page",
            "history_search",
            "history_around",
        ] {
            assert!(
                actions.contains(current),
                "session must expose current action {current}"
            );
        }
        let props = session["function"]["parameters"]["properties"]
            .as_object()
            .expect("session properties");
        assert!(props.contains_key("path"));
        assert!(!props.contains_key("key"));
        assert!(!props.contains_key("tool"));
    }

    #[test]
    fn get_agent_info_schema_exposes_capability_dimension() {
        let schemas = all_tool_schemas_with_env(|_| None);
        let get_agent_info =
            find_schema(&schemas, "get_agent_info").expect("get_agent_info schema must exist");
        let properties = get_agent_info["function"]["parameters"]["properties"]
            .as_object()
            .expect("get_agent_info properties must be an object");
        let dimension = properties
            .get("dimension")
            .expect("get_agent_info should expose dimension");
        let enum_values = dimension["enum"]
            .as_array()
            .expect("dimension should have enum values")
            .iter()
            .filter_map(Value::as_str)
            .collect::<std::collections::HashSet<_>>();

        assert!(enum_values.contains("identity"));
        assert!(enum_values.contains("capability"));
        assert!(enum_values.contains("all"));
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
    fn read_file_schema_exposes_only_line_range_contract() {
        let schemas = all_tool_schemas_with_env(|_| None);
        let read_file = find_schema(&schemas, "read_file").expect("read_file schema must exist");
        let func = read_file
            .get("function")
            .expect("read_file schema must include function block");
        let desc = func
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or_default();
        assert!(
            desc.contains("Do not use limit/offset/length")
                && desc.contains("start_line=1")
                && desc.contains("end_line=N"),
            "read_file description must advertise the current line-range contract: {desc}"
        );

        let params = func
            .get("parameters")
            .expect("read_file schema must include parameters");
        assert_eq!(
            params.get("additionalProperties").and_then(Value::as_bool),
            Some(false),
            "read_file should reject unknown top-level fields"
        );

        let properties = params
            .get("properties")
            .and_then(Value::as_object)
            .expect("read_file schema properties must be an object");
        for name in ["path", "start_line", "end_line", "outline"] {
            assert!(
                properties.contains_key(name),
                "read_file schema should expose `{name}`"
            );
        }
        for legacy in ["offset", "limit", "length", "count"] {
            assert!(
                !properties.contains_key(legacy),
                "read_file schema must not expose legacy/count field `{legacy}`"
            );
        }
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

        assert_eq!(
            params.get("additionalProperties").and_then(Value::as_bool),
            Some(false),
            "write_file should reject unknown top-level fields"
        );

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

    #[test]
    fn str_replace_schema_uses_provider_compatible_edit_mode_contract() {
        let schemas = all_tool_schemas_with_env(|_| None);
        let str_replace =
            find_schema(&schemas, "str_replace").expect("str_replace schema must exist");
        let desc = str_replace
            .pointer("/function/description")
            .and_then(Value::as_str)
            .expect("str_replace schema must include description");
        let params = str_replace
            .pointer("/function/parameters")
            .expect("str_replace schema must include parameters");

        assert!(
            desc.contains("Do not use aliases"),
            "str_replace description should steer models away from retired alias fields: {desc}"
        );
        assert_eq!(
            params.get("additionalProperties").and_then(Value::as_bool),
            Some(false),
            "str_replace should reject unknown top-level fields"
        );
        assert!(
            params.get("oneOf").is_none()
                && params.get("allOf").is_none()
                && params.get("anyOf").is_none(),
            "str_replace parameters must avoid provider-rejected top-level schema composition"
        );

        assert!(
            params.get("required").is_none(),
            "str_replace cannot require top-level path because multi-file batch mode puts path inside edits[]"
        );

        let per_action = params.get("x-astra-per-action-required").expect(
            "str_replace must use x-astra-per-action-required to encode edit-mode requirements",
        );
        let single = per_action
            .get("single")
            .and_then(Value::as_array)
            .expect("single mode must be listed");
        assert!(
            ["path", "old_str", "new_str"]
                .iter()
                .all(|field| single.iter().any(|value| value.as_str() == Some(*field))),
            "single mode must require path, old_str, and new_str: {single:?}"
        );
        let batch_same_file = per_action
            .get("batch_same_file")
            .and_then(Value::as_array)
            .expect("same-file batch mode must be listed");
        assert!(
            ["path", "edits"].iter().all(|field| batch_same_file
                .iter()
                .any(|value| value.as_str() == Some(*field))),
            "same-file batch mode must require path and edits: {batch_same_file:?}"
        );
        let batch_multi_file = per_action
            .get("batch_multi_file")
            .and_then(Value::as_array)
            .expect("multi-file batch mode must be listed");
        assert!(
            ["edits[].path", "edits[].old_str", "edits[].new_str"]
                .iter()
                .all(|field| batch_multi_file
                    .iter()
                    .any(|value| value.as_str() == Some(*field))),
            "multi-file batch mode must require path inside each edit: {batch_multi_file:?}"
        );

        assert_eq!(
            params
                .pointer("/properties/edits/items/additionalProperties")
                .and_then(Value::as_bool),
            Some(false),
            "batch edit entries should reject unknown fields"
        );
        assert!(
            params
                .pointer("/properties/edits/items/properties/path")
                .is_some(),
            "batch edit entries should advertise optional per-edit path"
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
