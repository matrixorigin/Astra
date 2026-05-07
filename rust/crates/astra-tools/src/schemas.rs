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
                    "timeout": {"type": "number", "description": "Timeout in seconds (default 30)"}
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
                "description": "Execute a shell command in the project root. Use for builds, tests, installs, and other CLI tasks. Avoid bash for operations with dedicated tools: read files → read_file (NOT cat/head/tail), search content → grep (NOT bash grep/rg), find files → glob (NOT bash find), list dirs → list_dir (NOT ls), edit files → str_replace (NOT sed/awk). FORBIDDEN for git inspection — NEVER use bash for `git status`, `git diff`, `git log`, `git show`, or similar git commands. Use git tool instead. Non-read-only bash does not participate in rollback journals; inside rollback-on-failure boundaries such as plan subtasks, run_chain, or explicit rollback-on-failure batch transactions, keep bash read-only and prefer structured tools such as write_file, git, or run_build_test. Can run curl, GitHub API, etc. Timeout varies (5-30s); override with timeout. On timeout or cancellation, returns any partial captured output plus a boundary note. Non-zero exits are returned as tool errors with stderr and exit code.",
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
                "description": "Read file contents with optional line range. Verify uncertain paths with list_dir/glob first. Large files (>80KB) require start_line/end_line or outline=true for signatures only. Output includes line numbers; pass content without line numbers to str_replace. Common images return a data URI; binary files are refused.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "File path relative to project root"},
                        "start_line": {"type": "integer", "minimum": 1, "description": "First line to read (1-based, optional)"},
                        "end_line": {"type": "integer", "minimum": 1, "description": "Last line to read (inclusive, optional)"},
                        "outline": {"type": "boolean", "description": "If true, return only function/class/struct/trait signatures with line numbers instead of full content. Ideal for understanding file structure."},
                        "transaction_id": {"type": "string", "description": "Optional explicit batch transaction id. Consecutive tool calls in the same batch with the same id and rollback_on_failure=true execute as one rollback boundary."},
                        "rollback_on_failure": {"type": "boolean", "description": "Optional explicit batch transaction flag. When true with transaction_id, a later failure inside the same contiguous batch transaction rolls back bounded file/database side effects recorded since the transaction began."}
                    },
                    "required": ["path"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "write_file",
                "description": "Create or overwrite a file. Use str_replace to edit existing files. Set delete=true to delete the file instead of writing.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "File path relative to project root"},
                        "content": {"type": "string", "description": "File content (not required when delete=true)"},
                        "delete": {"type": "boolean", "description": "If true, delete the file at path instead of writing. Refuses to delete directories, .git/ contents, or paths outside the project root."},
                        "transaction_id": {"type": "string", "description": "Optional explicit batch transaction id. Consecutive tool calls in the same batch with the same id and rollback_on_failure=true execute as one rollback boundary."},
                        "rollback_on_failure": {"type": "boolean", "description": "Optional explicit batch transaction flag. When true with transaction_id, a later failure inside the same contiguous batch transaction rolls back bounded file/database side effects recorded since the transaction began."}
                    },
                    "required": ["path"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "str_replace",
                "description": "Replace a string in a file. Tries an exact match first, then a unique quote/whitespace-aware fuzzy match when old_str differs slightly. On mismatch, shows closest matches with line numbers so you can fix and retry. Set dry_run=true to preview the diff without applying. For multiple edits to the same file, use 'edits' array instead of old_str/new_str — all edits apply atomically (all-or-nothing).",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "File path relative to project root"},
                        "old_str": {"type": "string", "description": "Exact string to replace (single-edit mode)"},
                        "new_str": {"type": "string", "description": "Replacement string (single-edit mode)"},
                        "edits": {
                            "type": "array",
                            "description": "Array of {old_str, new_str} pairs to apply atomically in order. If any fails, none are applied. More token-efficient than sequential calls. Mutually exclusive with old_str/new_str.",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "old_str": {"type": "string", "description": "Exact string to replace"},
                                    "new_str": {"type": "string", "description": "Replacement string"}
                                },
                                "required": ["old_str", "new_str"]
                            }
                        },
                        "dry_run": {"type": "boolean", "description": "If true, show unified diff without applying changes (default: false)"},
                        "replace_all": {"type": "boolean", "description": "If true, replace ALL occurrences of old_str (default: false, requires unique match)"},
                        "transaction_id": {"type": "string", "description": "Optional explicit batch transaction id. Consecutive tool calls in the same batch with the same id and rollback_on_failure=true execute as one rollback boundary."},
                        "rollback_on_failure": {"type": "boolean", "description": "Optional explicit batch transaction flag. When true with transaction_id, a later failure inside the same contiguous batch transaction rolls back bounded file/database side effects recorded since the transaction began."}
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
                "description": "Search for a pattern in files. Returns matching lines with file:line context (output truncated to ~10KB, 100 lines max by default). Supports content/files_with_matches/count modes, pagination via offset, optional scope_context annotations, and respects .gitignore/.astraignore when present. For large codebases, narrow path or pattern for complete results.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "pattern": {"type": "string", "description": "Regex pattern to search for"},
                        "path": {"type": "string", "description": "Directory or file to search (default: project root)"},
                        "include": {"type": "string", "description": "File glob filter e.g. '*.rs'"},
                        "glob": {"type": "string", "description": "Alias of include using ripgrep-style glob syntax, e.g. '**/*.rs'"},
                        "type": {"type": "string", "description": "Common file type filter, e.g. rust, python, typescript, tsx, javascript, jsx, go, java, c, cpp, ruby, shell, json, yaml, toml, markdown"},
                        "case_sensitive": {"type": "boolean", "description": "Case sensitive (default false)"},
                        "fixed_strings": {"type": "boolean", "description": "Treat pattern as a literal string instead of a regex (like grep -F / rg -F)."},
                        "word_match": {"type": "boolean", "description": "Require whole-word matches only (like grep -w / rg -w)."},
                        "context_lines": {"type": "integer", "description": "Lines of context before and after each match (like grep -C)"},
                        "before_context_lines": {"type": "integer", "description": "Lines of context before each match (like grep -B). Overrides the 'before' side of context_lines when provided."},
                        "after_context_lines": {"type": "integer", "description": "Lines of context after each match (like grep -A). Overrides the 'after' side of context_lines when provided."},
                        "max_matches": {"type": "integer", "description": "Max matches per file (limits output, saves tokens)"},
                        "multiline": {"type": "boolean", "description": "Allow regex matches to span newlines within a file. Useful for block patterns and multi-line structures."},
                        "scope_context": {"type": "boolean", "description": "Annotate each match with its containing function/class name (tree-sitter)"},
                        "output_mode": {"type": "string", "enum": ["content", "files_with_matches", "count"], "description": "Output mode: 'content' (default, matching lines), 'files_with_matches' (file paths only), 'count' (match counts per file)"},
                        "sort_by": {"type": "string", "enum": ["mtime", "path"], "description": "Sort matching files by newest modified time first (default 'mtime') or alphabetically by path."},
                        "offset": {"type": "integer", "minimum": 0, "description": "Skip first N result lines (for pagination)"},
                        "head_limit": {"type": "integer", "minimum": 0, "description": "Max result lines to return after offset. Defaults to 100; set 0 for unlimited."}
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
                "description": "Language Server Protocol operations. Requires a running LSP server for the target language. Operations: goto_definition, find_references, rename (symbol across files), hover (type info), call_hierarchy/incoming_calls/outgoing_calls, supertypes/subtypes (type hierarchy), implementation, declaration, type_definition, document_symbols, workspace_symbols, code_actions (quick fixes, refactors), completions, signature_help, diagnostics, format_document/format_range/format_on_type, code_lenses, prepare_rename, document_highlight, document_links, inlay_hints, folding_ranges, semantic_tokens, selection_ranges, linked_editing_range, document_colors/color_presentations. Use action_index for code_actions apply, item_index for completions/code_lenses resolve/execute, dry_run=false to apply writes. Without a file, diagnostics reports backend availability.",
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
                            ],
                            "description": "LSP operation to perform"
                        },
                        "file": {"type": "string", "description": "File path (required for most operations)"},
                        "line": {"type": "integer", "description": "1-based line number (position-based ops)"},
                        "column": {"type": "integer", "description": "1-based column (position-based ops)"},
                        "end_line": {"type": "integer", "description": "End line (range ops like format_range)"},
                        "end_column": {"type": "integer", "description": "End column (range ops)"},
                        "trigger_character": {"type": "string", "description": "Trigger char (format_on_type)"},
                        "symbol": {"type": "string", "description": "Symbol name (alternative to line/column)"},
                        "query": {"type": "string", "description": "Query for workspace_symbols"},
                        "new_name": {"type": "string", "description": "New name for rename"},
                        "dry_run": {"type": "boolean", "description": "Preview mode (default true). Set false to apply rename/format/code_action/completion/code_lens."},
                        "action_index": {"type": "integer", "minimum": 0, "description": "Code action index to apply (default 0)"},
                        "item_index": {"type": "integer", "minimum": 0, "description": "Item index for completions/code_lenses resolve or execute"},
                        "scope": {"type": "string", "enum": ["file", "project"], "description": "Operation scope (default: file)"},
                        "include_body": {"type": "boolean", "description": "Include function bodies (default false)"}
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
                        "action": {"type": "string", "enum": ["status","diff","log","show","blame","file_history","log_search","contributors","commit","revert_commit","stash","checkout_file","worktree"], "description": "Git operation to perform"},
                        "file": {"type": "string", "description": "File path (for blame, file_history)"},
                        "ref": {"type": "string", "description": "Git ref (for show, diff)"},
                        "n": {"type": "integer", "description": "Number of entries (for log)"},
                        "query": {"type": "string", "description": "Search query (for log_search)"},
                        "message": {"type": "string", "description": "Commit message (for commit)"}
                    },
                    "required": ["action"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "github",
                "description": "GitHub operations. Actions: list_prs, get_pr, ci_status, repo_stats, list_issues, get_issue, create_issue.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "action": {"type": "string", "enum": ["list_prs","get_pr","ci_status","repo_stats","list_issues","get_issue","create_issue"], "description": "GitHub operation"},
                        "owner": {"type": "string"},
                        "repo": {"type": "string"},
                        "number": {"type": "integer", "description": "PR or issue number"}
                    },
                    "required": ["action"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "memory",
                "description": "Memory operations. Actions: store, retrieve, purge, correct, profile, search, feedback.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "action": {"type": "string", "enum": ["store","retrieve","purge","correct","profile","search","feedback"], "description": "Memory operation"},
                        "content": {"type": "string", "description": "Content to store/correct"},
                        "query": {"type": "string", "description": "Query for retrieve/search"},
                        "memory_id": {"type": "string", "description": "ID for purge/correct/feedback"},
                        "memory_type": {"type": "string", "description": "Type: semantic, profile, procedural, working, episodic"}
                    },
                    "required": ["action"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "session",
                "description": "Session state and lifecycle operations. Actions: config, prioritize, deprioritize, set_goal, compact, rollback_edits, ask_user, sleep, tool_search.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "action": {"type": "string", "enum": ["config","prioritize","deprioritize","set_goal","compact","rollback_edits","ask_user","sleep","tool_search"], "description": "Session operation"},
                        "key": {"type": "string", "description": "Config key (for config)"},
                        "value": {"type": "string", "description": "Config value"},
                        "tool": {"type": "string", "description": "Tool name (for prioritize/deprioritize)"},
                        "goal": {"type": "string", "description": "Goal text (for set_goal)"},
                        "scope": {"type": "string", "enum": ["current_turn","turn","file","list"], "description": "Rollback scope (for rollback_edits)"},
                        "path": {"type": "string", "description": "File path (for rollback_edits scope=file)"},
                        "turn_index": {"type": "integer", "description": "Turn index (for rollback_edits scope=turn)"},
                        "question": {"type": "string", "description": "Question text (for ask_user)"},
                        "choices": {"type": "array", "items": {"type": "string"}, "description": "Multiple choice options 2-9 (for ask_user)"},
                        "default": {"type": "string", "description": "Default answer (for ask_user)"},
                        "context": {"type": "string", "description": "Brief context (for ask_user)"},
                        "duration_ms": {"type": "integer", "description": "Sleep duration in ms, max 300000 (for sleep)"},
                        "reason": {"type": "string", "description": "Reason for sleeping (for sleep)"},
                        "query": {"type": "string", "description": "Search query (for tool_search)"},
                        "max_results": {"type": "integer", "description": "Max results (for tool_search, default 5)"}
                    },
                    "required": ["action"]
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
                    "required": ["action"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "agent",
                "description": "Multi-agent operations. Actions: delegate, run_chain, spawn, get_result, send_message.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "action": {"type": "string", "enum": ["delegate","run_chain","spawn","get_result","send_message"], "description": "Agent operation"},
                        "task": {"type": "string", "description": "Task description (for delegate)"},
                        "steps": {"type": "array", "description": "Chain steps (for run_chain)"},
                        "description": {"type": "string", "description": "Short task description (for spawn)"},
                        "prompt": {"type": "string", "description": "Detailed task prompt (for spawn)"},
                        "agent_type": {"type": "string", "enum": ["explore","code-review","task","general-purpose"], "description": "Agent type (for spawn)"},
                        "model": {"type": "string", "description": "Optional model override (for spawn)"},
                        "background": {"type": "boolean", "description": "Return immediately with agent_id (for spawn, default false)"},
                        "name": {"type": "string", "description": "Addressable name for messaging (for spawn)"},
                        "max_turns": {"type": "integer", "description": "Max turns (for spawn)"},
                        "isolated": {"type": "boolean", "description": "Use isolated git worktree (for spawn)"},
                        "allowed_tools": {"type": "array", "items": {"type": "string"}, "description": "Tool allowlist (for spawn)"},
                        "max_output_tokens": {"type": "integer", "description": "Max output tokens (for spawn)"},
                        "inherit_prefix": {"type": "object", "description": "Inherit parent prompt-cache prefix (for spawn)"},
                        "agent_id": {"type": "string", "description": "Agent ID (for get_result)"},
                        "to": {"type": "string", "description": "Recipient agent_id or '*' (for send_message)"},
                        "message": {"description": "Message content (for send_message)"},
                        "summary": {"type": "string", "description": "Short preview of message (for send_message)"},
                        "message_type": {"type": "string", "enum": ["text","question","answer","instruction","progress","result","shutdown_request","shutdown_response"], "description": "Message type (for send_message)"},
                        "priority": {"type": "string", "enum": ["low","normal","high"], "description": "Message priority (for send_message)"},
                        "request_id": {"type": "string", "description": "Correlation ID (for send_message)"}
                    },
                    "required": ["action"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "introspect",
                "description": "Query own runtime state: token pressure, cache hit rate, tool health, alerts, working memory. Budget-adaptive detail.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "detail": {"type": "string", "enum": ["full","summary","minimal"], "description": "Output detail level (default: auto from budget)"}
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
        assert!(!names.contains(&"ask_user"));
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
}
