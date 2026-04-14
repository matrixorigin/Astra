//! Tool schema definitions for all edge tools.
//
//! Each schema is a JSON object following the OpenAI function-calling format:
//! `{ "type": "function", "function": { "name": ..., "description": ..., "parameters": ... } }`

use serde_json::{Value, json};

pub fn all_tool_schemas() -> Vec<Value> {
    vec![
        json!({
            "type": "function",
            "function": {
                "name": "bash",
                "description": "Execute a shell command in the project root. Use for builds, tests, installs, and other CLI tasks. FORBIDDEN for git inspection — NEVER use bash for `git status`, `git diff`, `git log`, `git show`, or similar git commands. Use git_status, git_diff, git_log, git_show tools instead. Non-read-only bash does not participate in rollback journals; inside rollback-on-failure boundaries such as plan subtasks, run_chain, or explicit rollback-on-failure batch transactions, keep bash read-only and prefer structured tools such as write_file, git_*, or run_build_test. Can run curl, GitHub API, etc. Timeout varies (5-30s); override with timeout.",
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
                "name": "powershell",
                "description": "Execute a PowerShell command in the project root. Use for Windows-oriented shell tasks, pwsh scripts, and cross-platform automation when PowerShell syntax is more appropriate than bash. Prefer git_* tools for git inspection instead of shelling out.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "command": {"type": "string", "description": "PowerShell command to run"},
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
                "description": "Read file contents with optional line range. Output includes line numbers (tab-separated). IMPORTANT: If you are NOT certain the file exists, use list_dir or glob FIRST to verify the path — do NOT guess paths. Large files (over ~80KB) will return an error when read without a range — use start_line/end_line or outline=true. For files over 500 lines, prefer targeted reads. Set outline=true to get only function/class/struct/trait signatures (saves tokens). If you previously read part of a file and request another range, the tool may auto-expand to return the full file to avoid fragmented reads. When using str_replace, provide old_str WITHOUT line numbers — only the actual file content.",
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
                "description": "Create or overwrite a file. Use str_replace to edit existing files.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "File path relative to project root"},
                        "content": {"type": "string", "description": "File content"},
                        "transaction_id": {"type": "string", "description": "Optional explicit batch transaction id. Consecutive tool calls in the same batch with the same id and rollback_on_failure=true execute as one rollback boundary."},
                        "rollback_on_failure": {"type": "boolean", "description": "Optional explicit batch transaction flag. When true with transaction_id, a later failure inside the same contiguous batch transaction rolls back bounded file/database side effects recorded since the transaction began."}
                    },
                    "required": ["path", "content"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "str_replace",
                "description": "Replace an exact string in a file. old_str must match exactly (including whitespace). On mismatch, shows closest matches with line numbers so you can fix and retry. Set dry_run=true to preview the diff without applying.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "File path relative to project root"},
                        "old_str": {"type": "string", "description": "Exact string to replace"},
                        "new_str": {"type": "string", "description": "Replacement string"},
                        "dry_run": {"type": "boolean", "description": "If true, show unified diff without applying changes (default: false)"},
                        "replace_all": {"type": "boolean", "description": "If true, replace ALL occurrences of old_str (default: false, requires unique match)"},
                        "transaction_id": {"type": "string", "description": "Optional explicit batch transaction id. Consecutive tool calls in the same batch with the same id and rollback_on_failure=true execute as one rollback boundary."},
                        "rollback_on_failure": {"type": "boolean", "description": "Optional explicit batch transaction flag. When true with transaction_id, a later failure inside the same contiguous batch transaction rolls back bounded file/database side effects recorded since the transaction began."}
                    },
                    "required": ["path", "old_str", "new_str"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "delete_file",
                "description": "Delete a file. Refuses to delete directories, .git/ contents, or paths outside the project root.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "File path relative to project root"},
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
                "name": "multi_edit",
                "description": "Apply multiple str_replace edits to a single file atomically. All edits must match; if any fails, none are applied. More token-efficient than sequential str_replace calls.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "File path relative to project root"},
                        "edits": {
                            "type": "array",
                            "description": "Array of {old_str, new_str} pairs to apply in order",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "old_str": {"type": "string", "description": "Exact string to replace"},
                                    "new_str": {"type": "string", "description": "Replacement string"}
                                },
                                "required": ["old_str", "new_str"]
                            }
                        },
                        "dry_run": {"type": "boolean", "description": "If true, show unified diff without applying (default: false)"},
                        "transaction_id": {"type": "string", "description": "Optional explicit batch transaction id. Consecutive tool calls in the same batch with the same id and rollback_on_failure=true execute as one rollback boundary."},
                        "rollback_on_failure": {"type": "boolean", "description": "Optional explicit batch transaction flag. When true with transaction_id, a later failure inside the same contiguous batch transaction rolls back bounded file/database side effects recorded since the transaction began."}
                    },
                    "required": ["path", "edits"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "rollback_file_edits",
                "description": "Revert file edits previously recorded in the undo journal. Use as the compensation action for recent write_file, str_replace, multi_edit, or workspace-edit changes. Can revert the current turn, a specific turn, the latest edit for one file, or list recorded edit entries.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "scope": {
                            "type": "string",
                            "enum": ["current_turn", "turn", "file", "list"],
                            "description": "Rollback scope. Defaults to current_turn. Use file to revert the latest recorded edit for one path, turn to revert a specific turn_index, or list to inspect journal entries."
                        },
                        "path": {
                            "type": "string",
                            "description": "File path relative to project root. Required when scope=file."
                        },
                        "turn_index": {
                            "type": "integer",
                            "minimum": 0,
                            "description": "Specific turn index to roll back. Required when scope=turn."
                        }
                    },
                    "required": []
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "rollback_database_snapshots",
                "description": "Restore MatrixOne pre-state snapshots captured by mutating mo_query calls. Use as the bounded compensation action for recent database mutations. Can restore the current turn, a specific turn, one explicit snapshot_id, or list recorded snapshot entries.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "scope": {
                            "type": "string",
                            "enum": ["current_turn", "turn", "snapshot", "list"],
                            "description": "Rollback scope. Defaults to current_turn. Use turn to restore one snapshot per database for a prior turn, snapshot to restore an explicit snapshot_id, or list to inspect recorded snapshot entries."
                        },
                        "turn_index": {
                            "type": "integer",
                            "minimum": 0,
                            "description": "Specific turn index to roll back. Required when scope=turn."
                        },
                        "snapshot_id": {
                            "type": "string",
                            "description": "Snapshot identifier captured from a mutating mo_query result or audit record. Required when scope=snapshot."
                        },
                        "database": {
                            "type": "string",
                            "description": "MatrixOne database to restore for scope=snapshot when the journal entry is unavailable. Defaults to the recorded database when present."
                        }
                    },
                    "required": []
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "rollback_session_state",
                "description": "Restore bounded session-local self-mod and task mutations recorded during this session. Use to roll back recent adjust_config, prioritize_tool, deprioritize_tool, set_goal, compress_context, task_create, task_update, or task_stop changes for the current turn, a specific turn, or to list recorded rollback handles.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "scope": {
                            "type": "string",
                            "enum": ["current_turn", "turn", "list"],
                            "description": "Rollback scope. Defaults to current_turn. Use turn to restore one prior turn's recorded session-state mutations, or list to inspect recorded rollback entries."
                        },
                        "turn_index": {
                            "type": "integer",
                            "minimum": 0,
                            "description": "Specific turn index to roll back. Required when scope=turn."
                        }
                    },
                    "required": []
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "rollback_turn_actions",
                "description": "Orchestrate bounded rollback across the shared file edit journal, MatrixOne snapshot journal, recorded git stash/git commit/git worktree rollback handles, and bounded session-state mutations for one turn. Use to revert mixed file/database/repo/session side effects from the current turn or a specific turn, or to list recorded rollback handles across all journals.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "scope": {
                            "type": "string",
                            "enum": ["current_turn", "turn", "list"],
                            "description": "Rollback scope. Defaults to current_turn. Use turn to revert one prior turn across all recorded journals, or list to inspect recorded rollback entries."
                        },
                        "turn_index": {
                            "type": "integer",
                            "minimum": 0,
                            "description": "Specific turn index to roll back. Required when scope=turn."
                        }
                    },
                    "required": []
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
                "description": "Search for a pattern in files. Returns matching lines with file:line context (output truncated to ~10KB, 100 lines max). Use scope_context=true to see which function/class each match is in. For large codebases, narrow path or pattern for complete results.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "pattern": {"type": "string", "description": "Regex pattern to search for"},
                        "path": {"type": "string", "description": "Directory or file to search (default: project root)"},
                        "include": {"type": "string", "description": "File glob filter e.g. '*.rs'"},
                        "case_sensitive": {"type": "boolean", "description": "Case sensitive (default false)"},
                        "context_lines": {"type": "integer", "description": "Lines of context before and after each match (like grep -C)"},
                        "max_matches": {"type": "integer", "description": "Max matches per file (limits output, saves tokens)"},
                        "scope_context": {"type": "boolean", "description": "Annotate each match with its containing function/class name (tree-sitter)"},
                        "output_mode": {"type": "string", "enum": ["content", "files_with_matches", "count"], "description": "Output mode: 'content' (default, matching lines), 'files_with_matches' (file paths only), 'count' (match counts per file)"},
                        "offset": {"type": "integer", "minimum": 0, "description": "Skip first N result lines (for pagination)"}
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
                "description": "Show git diff of working tree / index vs HEAD (or two-tree diff when `ref` is set without `path`). Supports commit ranges via `base_ref`..`ref` (e.g., base_ref:\"HEAD~5\" ref:\"HEAD\"). For commit review use git_show. Use stat_only:true for `git diff --stat`-style per-file line counts without full hunks — prefer this over bash.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "ref": {"type": "string", "description": "Git ref to diff against (optional). With `path`: diff working tree vs this ref for that path. Without `path`: diff between this ref and HEAD (two-tree). With `base_ref`: the tip of the range."},
                        "base_ref": {"type": "string", "description": "Base ref for range diff (optional). When set, produces `base_ref..ref` diff (e.g., base_ref:\"HEAD~5\" with ref:\"HEAD\"). If ref is omitted, defaults to HEAD as tip."},
                        "staged": {"type": "boolean", "description": "Show staged changes vs HEAD (default false). Do not combine with `ref`."},
                        "path": {"type": "string", "description": "Limit diff to one file (optional)"},
                        "stat_only": {"type": "boolean", "description": "If true, return only per-file insert/delete counts (--stat). Default false (full unified diff)."}
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
                "name": "git_show",
                "description": "Show a specific commit's full diff, message, author, and date. Use to review commits by SHA, inspect what a commit changed, or compare file changes in a specific commit. For working tree changes use git_diff instead.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "commit": {"type": "string", "description": "Commit SHA, branch, tag, or ref (e.g. HEAD, HEAD~1, abc1234)"},
                        "stat_only": {"type": "boolean", "description": "Show only file-level stats (no diff content). Default false."},
                        "file": {"type": "string", "description": "Scope output to a specific file path (optional)"}
                    },
                    "required": ["commit"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "git_blame",
                "description": "Show who last modified each line of a file, with commit info and dates. Use to understand code ownership and change history for specific lines.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "file": {"type": "string", "description": "File path relative to project root"},
                        "line_start": {"type": "integer", "description": "Start line number (optional)"},
                        "line_end": {"type": "integer", "description": "End line number (optional)"}
                    },
                    "required": ["file"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "git_file_history",
                "description": "Show change history for a specific file: who changed it, when, and why. Follows file renames.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "file": {"type": "string", "description": "File path relative to project root"},
                        "n": {"type": "integer", "description": "Number of commits (default 10)"}
                    },
                    "required": ["file"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "git_contributors",
                "description": "Analyze repository contributors, hot files (most changed), and recent activity. Provides structured insights about the team and codebase evolution.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "Limit analysis to a subdirectory (optional)"},
                        "since": {"type": "string", "description": "Time range e.g. '30 days ago', '2024-01-01' (optional)"}
                    },
                    "required": []
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "git_log_search",
                "description": "Semantic search on commit messages. Find commits by meaning, not just keywords — uses TF-IDF with CJK support. Example: 'when was auth refactored?' or '认证模块什么时候改的?'",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": {"type": "string", "description": "Natural language search query"},
                        "n": {"type": "integer", "description": "Max commits to search (default 200)"}
                    },
                    "required": ["query"]
                }
            }
        }),
        // ── Code Intelligence tools ────────────────────────────────────────
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
                "name": "git_commit",
                "description": "Stage files and create a git commit. Stages all changes by default. Use 'files' to stage specific files only.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "message": {"type": "string", "description": "Commit message (required)"},
                        "files": {"type": "array", "items": {"type": "string"}, "description": "Specific files to stage (optional; if omitted, stages all changes)"},
                        "all": {"type": "boolean", "description": "Stage all tracked changes (like git commit -a)"},
                        "transaction_id": {"type": "string", "description": "Optional explicit batch transaction id. Consecutive tool calls in the same batch with the same id and rollback_on_failure=true execute as one rollback boundary."},
                        "rollback_on_failure": {"type": "boolean", "description": "Optional explicit batch transaction flag. When true with transaction_id, a later failure inside the same contiguous batch transaction rolls back bounded file/database/repo-state side effects recorded since the transaction began."}
                    },
                    "required": ["message"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "git_revert_commit",
                "description": "Create a compensating revert commit for an earlier commit. Prefer the commit_sha returned by git_commit.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "commit_sha": {"type": "string", "description": "Full commit SHA or git revision to revert (required). Prefer the commit_sha returned by git_commit."}
                    },
                    "required": ["commit_sha"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "git_stash",
                "description": "Save or restore working tree changes. Use to temporarily shelve changes before switching tasks.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "action": {"type": "string", "enum": ["push", "apply", "pop", "list", "drop"], "description": "Stash operation"},
                        "message": {"type": "string", "description": "Description for push (optional)"},
                        "index": {"type": "integer", "description": "Stash index for apply/pop/drop (default 0)"},
                        "stash_ref": {"type": "string", "description": "Exact stash selector or OID for apply. Prefer the stash_ref returned by a previous successful git_stash push."},
                        "transaction_id": {"type": "string", "description": "Optional explicit batch transaction id. Consecutive tool calls in the same batch with the same id and rollback_on_failure=true execute as one rollback boundary."},
                        "rollback_on_failure": {"type": "boolean", "description": "Optional explicit batch transaction flag. When true with transaction_id, a later failure inside the same contiguous batch transaction rolls back bounded file/database/repo-state side effects recorded since the transaction began."}
                    },
                    "required": ["action"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "git_checkout_file",
                "description": "Revert a file to its last committed state, discarding working tree changes. Use as undo for bad edits.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "File path to revert"},
                        "ref": {"type": "string", "description": "Restore from specific commit/ref (default: HEAD)"},
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
                "name": "git_worktree",
                "description": "Manage git worktrees for isolated parallel work. Actions: enter (create worktree and switch session into it), exit (leave worktree and restore original directory), add (create without switching), list (show all), remove (cleanup). Use 'enter' for session-scoped isolation; the session directory changes to the worktree. Clean worktrees created by enter/add are tracked for bounded rollback via rollback_turn_actions while unchanged; explicit remove or exit_action='remove' remains the manual destructive path.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "action": {"type": "string", "enum": ["enter", "exit", "add", "list", "remove"], "description": "Worktree operation: enter (create + switch), exit (leave session), add (create only), list (show all), remove (cleanup)"},
                        "branch": {"type": "string", "description": "Branch name for the worktree (required for enter/add)"},
                        "path": {"type": "string", "description": "Filesystem path for the worktree (auto-generated if omitted for add; required for remove)"},
                        "new_branch": {"type": "boolean", "description": "Create a new branch (true, default) or use existing branch (false)"},
                        "force": {"type": "boolean", "description": "Force removal even if worktree has changes (for remove)"},
                        "delete_branch": {"type": "boolean", "description": "Also delete the branch when removing worktree (for remove)"},
                        "exit_action": {"type": "string", "enum": ["keep", "remove"], "description": "For exit: 'keep' preserves worktree, 'remove' deletes it (default: keep)"},
                        "discard_changes": {"type": "boolean", "description": "For exit with remove: confirm discarding uncommitted changes"}
                    },
                    "required": ["action"]
                }
            }
        }),
        // ── Code navigation tools ──────────────────────────────────────────
        json!({
            "type": "function",
            "function": {
                "name": "find_definition",
                "description": "Find where a symbol (function, class, struct, trait, type) is defined across the codebase. Uses AST parsing for accurate results. More precise than grep for code navigation.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "symbol": {"type": "string", "description": "Symbol name to find (exact or regex)"},
                        "language": {"type": "string", "description": "Filter by language: rust, python, typescript, go, java, c, cpp, ruby (optional; auto-detected if omitted)"},
                        "path": {"type": "string", "description": "Limit search to a subdirectory (optional)"},
                        "file": {"type": "string", "description": "File where the symbol is used (optional). Enables import-aware resolution: imports in this file are analyzed to prioritize the most likely definition."}
                    },
                    "required": ["symbol"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "find_references",
                "description": "Find all usages/references to a symbol across the codebase. Combines grep for speed with AST validation for accuracy. Shows file:line for each reference, categorized as definition/import/call/usage.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "symbol": {"type": "string", "description": "Symbol name to search for (exact match)"},
                        "path": {"type": "string", "description": "Limit search to a subdirectory (optional)"},
                        "include": {"type": "string", "description": "File glob filter e.g. '*.rs' (optional)"},
                        "kind": {"type": "string", "enum": ["all", "definition", "call", "import"], "description": "Filter references by kind (default: all)"},
                        "validate": {"type": "boolean", "description": "AST-validate results to filter comments/strings (default: true). Set false for speed."}
                    },
                    "required": ["symbol"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "call_graph",
                "description": "Analyze function call relationships. Shows what a function calls (outgoing) and optionally what calls it (incoming/callers). With callers=true and scope='project', scans up to 300 files — can be slow on large codebases. Use scope='file' for fast single-file results.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "File path relative to project root"},
                        "symbol": {"type": "string", "description": "Symbol name to analyze (function/method name)"},
                        "start_line": {"type": "integer", "minimum": 1, "description": "Start line (alternative to symbol name)"},
                        "end_line": {"type": "integer", "minimum": 1, "description": "End line (alternative to symbol name)"},
                        "callers": {"type": "boolean", "description": "If true, also find functions that CALL this symbol (reverse call graph)."},
                        "scope": {"type": "string", "enum": ["file", "project"], "description": "Scope for caller search: 'file' (default, fast) or 'project' (cross-file, thorough). Only used with callers=true."}
                    },
                    "required": ["path"]
                }
            }
        }),
        // ── Rename Symbol tool ────────────────────────────────────────────
        json!({
            "type": "function",
            "function": {
                "name": "rename_symbol",
                "description": "Rename a symbol across all files in the project. Uses AST-validated find_references to identify real code references (not comments/strings), then applies word-boundary-safe replacements. Dry-run by default for safety.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "symbol": {"type": "string", "description": "Current name of the symbol to rename"},
                        "new_name": {"type": "string", "description": "New name for the symbol"},
                        "path": {"type": "string", "description": "Limit rename to a subdirectory (optional)"},
                        "include": {"type": "string", "description": "File glob filter e.g. '*.rs' (optional)"},
                        "dry_run": {"type": "boolean", "description": "Preview changes without applying (default: true). Set false to apply."},
                        "transaction_id": {"type": "string", "description": "Optional explicit batch transaction id. Consecutive tool calls in the same batch with the same id and rollback_on_failure=true execute as one rollback boundary."},
                        "rollback_on_failure": {"type": "boolean", "description": "Optional explicit batch transaction flag. When true with transaction_id, a later failure inside the same contiguous batch transaction rolls back bounded file/database side effects recorded since the transaction began."}
                    },
                    "required": ["symbol", "new_name"]
                }
            }
        }),
        // ── Dead Code Detection tool ─────────────────────────────────────
        json!({
            "type": "function",
            "function": {
                "name": "dead_code",
                "description": "Find potentially unused functions, types, and constants by cross-referencing definitions with project-wide usage. Reports symbols with zero external references. Useful before cleanup or to understand code coverage.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "File or directory to scan (default: current directory)"},
                        "include": {"type": "string", "description": "File glob filter e.g. '*.rs' (optional)"},
                        "kind": {"type": "string", "enum": ["all", "function", "type", "constant"], "description": "Filter by symbol kind (default: all)"}
                    }
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "extract_members",
                "description": "Extract fields, methods, and variants from a struct/class/enum/interface/trait definition. Returns typed member list with visibility and default values. Useful for understanding type structure, adding missing fields, or generating constructors.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "file": {"type": "string", "description": "File containing the type definition"},
                        "line": {"type": "integer", "description": "Line number within the type definition (any line inside the struct/class/enum)"}
                    },
                    "required": ["file", "line"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "type_hierarchy",
                "description": "Find implementation relationships: what traits/interfaces a type implements, or what types implement a given trait/interface. Searches across the project using AST parsing.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "name": {"type": "string", "description": "Trait or type name to search for"},
                        "direction": {"type": "string", "enum": ["implementations", "supertypes"], "description": "implementations: find types that implement this trait. supertypes: find traits this type implements. Default: implementations"},
                        "include": {"type": "string", "description": "File glob filter e.g. '*.rs' (optional)"}
                    },
                    "required": ["name"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "hover_info",
                "description": "Get comprehensive info about the symbol at a specific cursor position. Returns: symbol kind, signature, doc comment, scope breadcrumbs, member preview (for types), and usage count. Like an IDE hover tooltip. Use when you need full context about what's at a specific location.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "file": {"type": "string", "description": "File path"},
                        "line": {"type": "integer", "description": "Line number (1-indexed)"},
                        "column": {"type": "integer", "description": "Column number (0-indexed, default: 0)"}
                    },
                    "required": ["file", "line"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "symbol_search",
                "description": "Search for symbols (functions, types, methods) across the project by name. Like 'Go to Symbol in Workspace'. Faster and more precise than grep for finding code definitions. Supports fuzzy matching.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": {"type": "string", "description": "Symbol name or pattern to search for (case-insensitive substring match)"},
                        "kind": {"type": "string", "enum": ["all", "function", "type", "method", "constant"], "description": "Filter by symbol kind (default: all)"},
                        "include": {"type": "string", "description": "File glob filter e.g. '*.rs' (optional)"},
                        "limit": {"type": "integer", "description": "Max results (default: 20)"}
                    },
                    "required": ["query"]
                }
            }
        }),
        // ── Build/Test tool ─────────────────────────────────────────────
        json!({
            "type": "function",
            "function": {
                "name": "run_build_test",
                "description": "Run a build or test command with structured error parsing. Returns structured errors with file:line:col locations AND auto-reads surrounding source code for each error location, so you can fix issues in one shot without additional read_file calls. Use this instead of raw bash for build/test commands. Set auto_fix=true to automatically apply trivial fixes (unused imports/variables) and re-run. Set report_only=true to preview what auto-fix would do without applying. Note: has side effects (builds artifacts, updates caches) — results are never cached.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "command": {"type": "string", "description": "Build/test command to run (e.g., 'cargo test', 'pytest', 'npm test')"},
                        "context_lines": {"type": "integer", "description": "Lines of source context around each error (default: 5)"},
                        "auto_fix": {"type": "boolean", "description": "If true, automatically apply high-confidence trivial fixes and re-run (max 3 iterations). Only fixes with confidence >= 0.8 are applied. Default: false"},
                        "abort_on_regression": {"type": "boolean", "description": "If true (default), abort auto-fix loop and revert changes when error count increases. Prevents fix attempts from making things worse."},
                        "report_only": {"type": "boolean", "description": "If true, show what auto-fix would do without actually applying fixes. Useful for previewing changes before committing to them. Default: false"}
                    },
                    "required": ["command"]
                }
            }
        }),
        // ── MatrixOne tools ─────────────────────────────────────────────
        json!({
            "type": "function",
            "function": {
                "name": "mo_query",
                "description": "Execute a SQL query against MatrixOne database. Returns formatted table results (truncated to ~20KB). Destructive operations (DELETE, DROP, TRUNCATE) are blocked by default — pass allow_destructive=true to confirm. Mutating queries capture a pre-state snapshot before execution so rollback hints can reference a concrete snapshot. Use for data exploration, schema inspection, analytics queries, and bounded writes.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "sql": {"type": "string", "description": "SQL query to execute"},
                        "database": {"type": "string", "description": "Database name (default: MATRIXONE_DATABASE_PREFIX + MATRIXONE_DATABASE from env)"},
                        "transaction_id": {"type": "string", "description": "Optional explicit batch transaction id. Consecutive tool calls in the same batch with the same id and rollback_on_failure=true execute as one rollback boundary."},
                        "rollback_on_failure": {"type": "boolean", "description": "Optional explicit batch transaction flag. When true with transaction_id, a later failure inside the same contiguous batch transaction rolls back bounded file/database side effects recorded since the transaction began."}
                    },
                    "required": ["sql"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "mo_snapshot",
                "description": "Manage MatrixOne data snapshots for point-in-time recovery and experiment isolation. Actions: create (name a checkpoint), list (show all), drop (remove), restore (rollback to snapshot). Create and restore can target a specific database when needed.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "action": {"type": "string", "enum": ["create", "list", "drop", "restore"], "description": "Snapshot operation"},
                        "name": {"type": "string", "description": "Snapshot name (required for create/drop/restore)"},
                        "database": {"type": "string", "description": "Database name for create/restore (default: from MATRIXONE_DATABASE env)"}
                    },
                    "required": ["action"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "mo_branch",
                "description": "Coordinate git branches with MatrixOne data branches. Creates data snapshots aligned with git branches for experiment isolation. Actions: list (show git + data branches), create (create data branch), sync (check alignment).",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "action": {"type": "string", "enum": ["list", "create", "sync"], "description": "Branch operation"},
                        "name": {"type": "string", "description": "Branch/snapshot name (optional for create — auto-generates from git branch)"}
                    },
                    "required": ["action"]
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
                "name": "github_repo_stats",
                "description": "Get repository statistics and metadata from GitHub. Use for stars, forks, watchers, open issues, last push time, language, project overview, or repo stats. repo can be 'owner/repo' or a bare project name that will be auto-resolved when safe. detail: 'brief' (default) returns stars/forks/open_issues/language/pushed_at; 'normal' adds watchers/default_branch/topics/license; 'detailed' and 'full' keep the same fields with larger text budgets for description.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "repo": {"type": "string", "description": "Repository as 'owner/repo' or bare project name"},
                        "detail": {"type": "string", "enum": ["brief", "normal", "detailed", "full"], "description": "Output detail level: brief (default), normal, detailed, or full"}
                    },
                    "required": ["repo"]
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
                        "memory_type": {"type": "string", "description": "Type: semantic (default), profile, procedural, working"},
                        "trust_tier": {"type": "string", "description": "Confidence tier: T1 (user-verified, 365d), T2 (curated, 180d), T3 (inferred, 60d), T4 (speculative, 30d). Default T3."},
                        "session_id": {"type": "string", "description": "Session ID for grouping. Omit to use current session."}
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
                        "top_k": {"type": "integer", "description": "Max results (default 10)"},
                        "min_confidence": {"type": "number", "description": "Minimum confidence threshold 0.0-1.0 (default 0.3). Filters low-quality results."}
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
                "name": "memory_correct",
                "description": "Correct or update an existing memory. Use when user says a stored memory is wrong or needs updating.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "memory_id": {"type": "string", "description": "ID of the memory to correct"},
                        "new_content": {"type": "string", "description": "Updated content for the memory"},
                        "reason": {"type": "string", "description": "Reason for the correction"}
                    },
                    "required": ["memory_id", "new_content"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "memory_profile",
                "description": "Retrieve user profile, preferences, and habits stored in memory. Use when user asks about their preferences or you need to personalize behavior.",
                "parameters": {
                    "type": "object",
                    "properties": {},
                    "required": []
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "adjust_config",
                "description": "Adjust a bounded runtime config value during this session. Uses a governor with per-turn mutation and drift limits, and records the previous value so rollback_session_state or rollback_turn_actions can restore it within the same session.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "enum": [
                                "compression.compression_threshold",
                                "memory.retrieval_top_k",
                                "tool_selection.max_tools",
                                "tool_selection.tool_budget_tokens",
                                "token_budget.max_turn_input_tokens",
                                "token_budget.tools_reserve",
                                "verification.strictness"
                            ],
                            "description": "Which config path to adjust"
                        },
                        "value": {
                            "description": "New value for the path (number/integer depending on the field)"
                        },
                        "force": {
                            "type": "boolean",
                            "description": "Override drift or mutation governor once when true"
                        }
                    },
                    "required": ["path", "value"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "prioritize_tool",
                "description": "Pin a tool as preferred for this session. Removes it from the deprioritized set and records prior tool preferences for bounded session-state rollback.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "tool": {"type": "string", "description": "Tool name to prioritize"}
                    },
                    "required": ["tool"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "deprioritize_tool",
                "description": "Mark a tool as deprioritized for this session. Removes it from the pinned set and records prior tool preferences for bounded session-state rollback.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "tool": {"type": "string", "description": "Tool name to deprioritize"}
                    },
                    "required": ["tool"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "set_goal",
                "description": "Set or replace the session goal used by goal tracking and self-awareness. Records the prior goal-tracking snapshot so rollback_session_state can restore it within the same session.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "goal": {"type": "string", "description": "Goal statement for the current session"}
                    },
                    "required": ["goal"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "compress_context",
                "description": "Record a manual context compression request for this session. Records the previous in-memory compression state so rollback_session_state can restore that session-local state.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "reason": {"type": "string", "description": "Optional reason for manual compression"}
                    },
                    "required": []
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "web_fetch",
                "description": "Fetch a URL and return its content (truncated to ~10KB by default). Use for reading web pages, APIs, documentation, or any HTTP resource. Safer and simpler than bash+curl. Set max_bytes to fetch more content.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "url": {"type": "string", "description": "URL to fetch (http:// or https://)"},
                        "max_bytes": {"type": "integer", "description": "Max response size in bytes (default 10000)"},
                        "timeout": {"type": "integer", "description": "Timeout in seconds (default 10)"}
                    },
                    "required": ["url"]
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
                            "enum": ["capability", "state", "goals", "memory", "identity", "context_snapshot", "context_trend", "snapshot", "reflect", "profile", "goal", "trace", "budget", "signals", "health", "journal", "verify", "all"],
                            "description": "Which dimension to query. When a session id is wired, the persistent self surfaces are the source of truth: 'snapshot', 'reflect', 'profile', 'goal', 'trace', 'budget', 'signals', 'health', 'journal', 'verify', and legacy views like 'capability', 'state', 'goals', 'context_snapshot', 'context_trend', 'identity', 'all' are compatibility aliases onto that persisted state."
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
                "description": "Diagnose past tool execution: why a tool failed, why results were unexpected, why a specific tool was or wasn't selected, performance bottlenecks. When a live session id is available this reconstructs a local liquid reflection view from persistent self state; otherwise it falls back to the server-backed /reflect path. Call this when asked to diagnose, debug, or explain past behavior.",
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
        json!({
            "type": "function",
            "function": {
                "name": "context_analysis",
                "description": "Deep analysis of context window composition, token allocation, and budget pressure. Modes: 'turn' — hierarchical breakdown of a single turn (system prompt sub-components, history, memory, tools with proportional percentages). 'session' — multi-turn aggregation with trends, compression events, peak/average stats. 'compare' — side-by-side delta between two turns.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "mode": {
                            "type": "string",
                            "enum": ["turn", "session", "compare"],
                            "description": "Analysis mode. 'turn': single-turn deep breakdown. 'session': multi-turn trends. 'compare': diff two turns."
                        },
                        "turn": {
                            "type": "integer",
                            "description": "Turn number (1-based, -1 for latest). Used with mode='turn'."
                        },
                        "turn_a": {
                            "type": "integer",
                            "description": "First turn for comparison (1-based). Used with mode='compare'."
                        },
                        "turn_b": {
                            "type": "integer",
                            "description": "Second turn for comparison (1-based, -1 for latest). Used with mode='compare'."
                        }
                    },
                    "required": []
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "run_chain",
                "description": "Execute a multi-step tool chain. Each step runs a tool and passes its output to the next step via variable substitution ($prev for previous output, $step.{key} for named step output, $input.{key} for original input). Stops on first error. Optionally enable rollback_on_failure to automatically revert bounded file/database side effects produced inside the chain when a later step fails. In rollback_on_failure chains, keep bash read-only because arbitrary shell mutations do not participate in rollback; prefer structured mutation tools or run_build_test when available. Use for complex multi-tool workflows like: find files → read contents → analyze.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "name": {"type": "string", "description": "Chain name for logging"},
                        "description": {"type": "string", "description": "What this chain does"},
                        "rollback_on_failure": {"type": "boolean", "description": "When true, automatically invokes bounded rollback for file/database side effects recorded inside this chain if a later step fails."},
                        "steps": {
                            "type": "array",
                            "description": "Ordered list of tool invocations",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "tool": {"type": "string", "description": "Tool name to execute"},
                                    "args": {"type": "object", "description": "Tool arguments. Use $prev, $step.key, $input.key for variable substitution"},
                                    "output_key": {"type": "string", "description": "Optional key to reference this step's output later via $step.{key}"},
                                    "skip_if_prev_contains": {"type": "string", "description": "Skip this step if previous output contains this string"}
                                },
                                "required": ["tool", "args"]
                            }
                        },
                        "input": {"type": "object", "description": "Initial input variables accessible via $input.{key}"}
                    },
                    "required": ["name", "steps"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "ask_user",
                "description": "Ask the user a question and wait for their response. Use when you need clarification, user preferences, or decisions during execution. Supports both multiple choice and free-form questions. The user can always provide custom text even for multiple choice questions.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "question": {
                            "type": "string",
                            "description": "The question to ask the user. Be clear and specific."
                        },
                        "choices": {
                            "type": "array",
                            "items": {"type": "string"},
                            "description": "Optional list of choices for multiple choice (2-9 options). User can always provide custom text. Omit for free-form questions."
                        },
                        "default": {
                            "type": "string",
                            "description": "Optional default answer if user presses Enter without input."
                        },
                        "context": {
                            "type": "string",
                            "description": "Optional brief context about why you're asking this question."
                        }
                    },
                    "required": ["question"]
                }
            }
        }),
        // ─── Task management tools ────────────────────────────────────────────
        json!({
            "type": "function",
            "function": {
                "name": "task_create",
                "description": "Create a structured task for tracking complex multi-step work. Use proactively when: (1) task requires 3+ distinct steps, (2) plan mode is active, (3) user provides multiple tasks. Skip for single trivial tasks. Successful mutations record a bounded task-state rollback handle.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "title": {
                            "type": "string",
                            "description": "Brief, actionable title in imperative form (e.g., 'Fix authentication bug in login flow')"
                        },
                        "description": {
                            "type": "string",
                            "description": "What needs to be done - detailed requirements and context"
                        },
                        "subtasks": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "id": {"type": "string", "description": "Unique subtask ID (e.g., 'setup-db', 'add-tests')"},
                                    "title": {"type": "string"},
                                    "description": {"type": "string"},
                                    "depends_on": {
                                        "type": "array",
                                        "items": {"type": "string"},
                                        "description": "IDs of subtasks that must complete first"
                                    }
                                },
                                "required": ["id", "title"]
                            },
                            "description": "Optional breakdown into subtasks with dependencies"
                        }
                    },
                    "required": ["title"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "task_list",
                "description": "List all tasks in the current session. Use to: (1) see available work, (2) check overall progress, (3) find blocked tasks needing attention.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "status": {
                            "type": "string",
                            "enum": ["all", "pending", "in_progress", "completed", "active"],
                            "description": "Filter by status. 'active' = pending + in_progress. Default: 'all'"
                        }
                    }
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "task_get",
                "description": "Get full details of a task by ID, including description, subtasks, and dependencies.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "task_id": {
                            "type": "string",
                            "description": "The task ID to retrieve"
                        }
                    },
                    "required": ["task_id"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "task_update",
                "description": "Update a task's status or progress. Always mark task as 'in_progress' BEFORE starting work, then 'completed' when done. Successful mutations record a bounded task-state rollback handle.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "task_id": {
                            "type": "string",
                            "description": "The task ID to update"
                        },
                        "status": {
                            "type": "string",
                            "enum": ["pending", "in_progress", "completed", "failed", "cancelled"],
                            "description": "New status for the task"
                        },
                        "subtask_id": {
                            "type": "string",
                            "description": "If provided, update this subtask instead of the main task"
                        },
                        "error_message": {
                            "type": "string",
                            "description": "Error details if status is 'failed'"
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
                "description": "Stop/cancel a running task. Use when a task needs to be aborted before completion. Successful mutations record a bounded task-state rollback handle.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "task_id": {
                            "type": "string",
                            "description": "The task ID to stop"
                        },
                        "reason": {
                            "type": "string",
                            "description": "Optional reason for stopping the task"
                        }
                    },
                    "required": ["task_id"]
                }
            }
        }),
        // ─── Sleep tool ───────────────────────────────────────────────────────
        json!({
            "type": "function",
            "function": {
                "name": "sleep",
                "description": "Wait for a specified duration. Use when waiting for external events, when the user asks you to pause, or when you have nothing to do. Prefer this over `bash(sleep ...)` as it doesn't hold a shell process.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "duration_ms": {
                            "type": "integer",
                            "description": "Duration to sleep in milliseconds (1000 = 1 second). Max 300000 (5 minutes)."
                        },
                        "reason": {
                            "type": "string",
                            "description": "Optional reason for sleeping (for logging)"
                        }
                    },
                    "required": ["duration_ms"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "tool_search",
                "description": "Search available tools by name or description keywords. Use to discover tools when unsure which one to use. Returns matching tool names with brief descriptions. Supports direct selection with 'select:tool_name' or keyword search.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "Search query: 'select:tool_name' for exact match, or keywords to search tool names/descriptions. E.g. 'git', 'file read', 'select:str_replace'"
                        },
                        "max_results": {
                            "type": "integer",
                            "description": "Maximum number of results to return (default: 5, max: 20)",
                            "default": 5
                        }
                    },
                    "required": ["query"]
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
                "name": "send_message",
                "description": "Send a message to another agent in the current delegation or team. Use for coordination, asking questions, reporting progress, or requesting approvals.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "to": {
                            "type": "string",
                            "description": "Recipient: agent_id of the target agent, or \"*\" for broadcast to all peers in the current delegation"
                        },
                        "message": {
                            "oneOf": [
                                { "type": "string", "description": "Plain text message" },
                                { "type": "object", "description": "Structured JSON message" }
                            ],
                            "description": "Message content"
                        },
                        "summary": {
                            "type": "string",
                            "description": "A 5-10 word summary shown as preview (recommended for long messages)"
                        },
                        "message_type": {
                            "type": "string",
                            "enum": ["text", "question", "answer", "instruction", "progress", "result", "shutdown_request", "shutdown_response"],
                            "default": "text",
                            "description": "Message type for structured handling"
                        },
                        "priority": {
                            "type": "string",
                            "enum": ["low", "normal", "high"],
                            "default": "normal",
                            "description": "Message priority"
                        },
                        "request_id": {
                            "type": "string",
                            "description": "Optional ID for request/response correlation"
                        }
                    },
                    "required": ["to", "message"]
                }
            }
        }),
        // ── spawn_agent: Dynamic agent spawning ─────────────────────────────────
        json!({
            "type": "function",
            "function": {
                "name": "spawn_agent",
                "description": "Launch a specialized sub-agent to perform a task. Agents run autonomously and return results. Use for parallel work, independent research, code review, or any task that benefits from dedicated focus. Agent types: 'explore' (fast codebase research), 'code-review' (analyze changes), 'task' (run commands), 'general-purpose' (full capabilities).",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "description": {
                            "type": "string",
                            "description": "A short (3-5 word) description of the task."
                        },
                        "prompt": {
                            "type": "string",
                            "description": "Detailed task prompt for the agent. Be specific about what you want."
                        },
                        "agent_type": {
                            "type": "string",
                            "enum": ["explore", "code-review", "task", "general-purpose"],
                            "description": "Type of specialized agent. 'explore' for research, 'code-review' for reviewing changes, 'task' for running commands, 'general-purpose' for complex multi-step tasks.",
                            "default": "general-purpose"
                        },
                        "model": {
                            "type": "string",
                            "description": "Optional model override (e.g., 'claude-sonnet', 'claude-opus', 'claude-haiku')."
                        },
                        "background": {
                            "type": "boolean",
                            "description": "Run in background (async). If true, returns immediately with agent_id. Default: true.",
                            "default": true
                        },
                        "name": {
                            "type": "string",
                            "description": "Name for agent-to-agent messaging. Makes agent addressable via send_message."
                        },
                        "max_turns": {
                            "type": "integer",
                            "description": "Max turns before stopping. Default varies by agent_type.",
                            "minimum": 1,
                            "maximum": 100
                        },
                        "isolated": {
                            "type": "boolean",
                            "description": "Create isolated git worktree for this agent.",
                            "default": false
                        },
                        "allowed_tools": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "Tool allowlist (overrides agent_type defaults)."
                        }
                    },
                    "required": ["description", "prompt"]
                }
            }
        }),
        // ─── Diagnose tool ────────────────────────────────────────────────────────
        json!({
            "type": "function",
            "function": {
                "name": "diagnose",
                "description": "Get system diagnostics and health information. Use when debugging issues, checking resource usage, or verifying tool availability. Returns system stats, environment info, and tool status.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "category": {
                            "type": "string",
                            "enum": ["all", "system", "environment", "tools", "tasks", "session"],
                            "description": "What to diagnose: 'all' for everything, 'system' for OS/resources, 'environment' for env vars, 'tools' for available tools, 'tasks' for task status, 'session' for current session info. Default: 'all'"
                        },
                        "verbose": {
                            "type": "boolean",
                            "description": "Include detailed information. Default: false",
                            "default": false
                        }
                    },
                    "required": []
                }
            }
        }),
        // ─── LSP tool: unified language server interface ─────────────────────────
        json!({
            "type": "function",
            "function": {
                "name": "lsp",
                "description": "Interact with Language Server Protocol for editor-grade code intelligence from an active language server. Prefer this over text search when you need symbol-aware navigation, autocomplete, quick fixes, auto-imports, signature help, diagnostics, rename/code actions, or other follow-up actions. Advanced editor-rendering operations such as document highlights/links, inlay hints, folding ranges, colors, semantic tokens, selection ranges, and linked editing are also available but are usually lower ROI unless the task explicitly needs IDE-style rendering details. Use action_index with code_actions apply, item_index to resolve or execute/apply a returned completion or code lens, and dry_run=false only for supported write operations. On Rust files, code_lenses first use native rust-analyzer textDocument/codeLens Run/Debug lenses when available; if standard LSP code lenses are empty, they can still fall back to rust-analyzer runnables. Rust hover can also include action links for runnable symbols (for example Run/Debug on tests) when the server provides them. Rust signature_help can include precise parameter label offsets, Rust completions can expose richer postfix/snippet-style candidates, and Rust code_actions can surface real assists such as import fixes. Both native and fallback Rust code lenses support item_index + dry_run=false execution. Without a file, diagnostics reports backend availability; with a file, diagnostics first tries textDocument/diagnostic and falls back to the latest publishDiagnostics snapshot.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "operation": {
                            "type": "string",
                            "enum": [
                                "goto_definition",
                                "find_references",
                                "hover",
                                "document_symbols",
                                "workspace_symbols",
                                "call_hierarchy",
                                "incoming_calls",
                                "outgoing_calls",
                                "declaration",
                                "type_definition",
                                "implementation",
                                "supertypes",
                                "subtypes",
                                "prepare_rename",
                                "rename",
                                "code_actions",
                                "completions",
                                "signature_help",
                                "document_highlight",
                                "document_links",
                                "inlay_hints",
                                "folding_ranges",
                                "document_colors",
                                "color_presentations",
                                "semantic_tokens",
                                "code_lenses",
                                "selection_ranges",
                                "linked_editing_range",
                                "format_document",
                                "format_range",
                                "format_on_type",
                                "diagnostics"
                            ],
                            "description": "LSP operation to perform. Prefer high-ROI operations like goto_definition/find_references/hover/completions/code_actions/signature_help/diagnostics first; editor-rendering operations like document_colors/color_presentations/semantic_tokens/folding_ranges/selection_ranges/linked_editing_range are lower ROI unless you explicitly need IDE-style view state."
                        },
                        "file": {
                            "type": "string",
                            "description": "File path. Required for almost all operations; diagnostics is the main operation that can omit it."
                        },
                        "line": {
                            "type": "integer",
                            "description": "Line number (1-based). Required for position-based operations."
                        },
                        "column": {
                            "type": "integer",
                            "description": "Column/character offset (1-based). Required for position-based operations."
                        },
                        "end_line": {
                            "type": "integer",
                            "description": "End line number (1-based). Required for range-based operations like format_range."
                        },
                        "end_column": {
                            "type": "integer",
                            "description": "End column/character offset (1-based). Required for range-based operations like format_range."
                        },
                        "trigger_character": {
                            "type": "string",
                            "description": "Typed character that triggered on-type formatting. Required for format_on_type."
                        },
                        "symbol": {
                            "type": "string",
                            "description": "Symbol name for symbol-based operations (alternative to line/column)"
                        },
                        "query": {
                            "type": "string",
                            "description": "Search query for workspace_symbols operation"
                        },
                        "new_name": {
                            "type": "string",
                            "description": "New identifier name for rename operations"
                        },
                        "dry_run": {
                            "type": "boolean",
                            "description": "Preview by default. Set false only to apply a supported rename, document/range/on-type format, code action edit, selected completion item, or selected code lens command/runnable."
                        },
                        "action_index": {
                            "type": "integer",
                            "minimum": 0,
                            "description": "For code_actions apply, choose which returned action to apply by index (default: 0)."
                        },
                        "item_index": {
                            "type": "integer",
                            "minimum": 0,
                            "description": "For completions or code_lenses, optionally choose a specific returned item by index to resolve in preview mode, or to apply/execute when dry_run=false."
                        },
                        "scope": {
                            "type": "string",
                            "enum": ["file", "project"],
                            "description": "Scope for certain operations (default: file)"
                        },
                        "include_body": {
                            "type": "boolean",
                            "description": "Include function bodies in results (default: false)"
                        }
                    },
                    "required": ["operation"]
                }
            }
        }),
        // ── Env tool: environment variable management ──────────────────────────────
        json!({
            "type": "function",
            "function": {
                "name": "env",
                "description": "Manage environment variables for the current session. List, get, set, unset, or search environment variables. Changes persist only for this session. Sensitive values (tokens, keys, passwords) are automatically masked in output.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "operation": {
                            "type": "string",
                            "enum": ["list", "get", "set", "unset", "search"],
                            "description": "Operation to perform: 'list' shows all vars, 'get' retrieves one var, 'set' creates/updates, 'unset' removes, 'search' finds vars by regex pattern"
                        },
                        "name": {
                            "type": "string",
                            "description": "Variable name (required for get/set/unset)"
                        },
                        "value": {
                            "type": "string",
                            "description": "Value to set (required for set operation)"
                        },
                        "pattern": {
                            "type": "string",
                            "description": "Regex pattern for search (case-insensitive)"
                        },
                        "show_values": {
                            "type": "boolean",
                            "description": "Show full values in list/search (default: false, shows char count instead)"
                        }
                    },
                    "required": ["operation"]
                }
            }
        }),
        // ── Notebook edit tool: Jupyter notebook editing ───────────────────────────
        json!({
            "type": "function",
            "function": {
                "name": "notebook_edit",
                "description": "Edit Jupyter notebook (.ipynb) cells. Replace, insert, or delete cells by ID. Supports code and markdown cell types. Use read_file first to view the notebook structure and cell IDs.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "notebook_path": {
                            "type": "string",
                            "description": "Path to the Jupyter notebook file (.ipynb)"
                        },
                        "cell_id": {
                            "type": "string",
                            "description": "Cell ID to edit. For insert mode, new cell is inserted after this cell. For replace/delete, this cell is modified."
                        },
                        "new_source": {
                            "type": "string",
                            "description": "New source content for the cell (required for replace/insert)"
                        },
                        "cell_type": {
                            "type": "string",
                            "enum": ["code", "markdown"],
                            "description": "Cell type. Required for insert, optional for replace (defaults to existing type)"
                        },
                        "edit_mode": {
                            "type": "string",
                            "enum": ["replace", "insert", "delete"],
                            "description": "Edit operation: 'replace' updates existing cell, 'insert' adds new cell after cell_id, 'delete' removes cell. Default: replace"
                        },
                        "transaction_id": {"type": "string", "description": "Optional explicit batch transaction id. Consecutive tool calls in the same batch with the same id and rollback_on_failure=true execute as one rollback boundary."},
                        "rollback_on_failure": {"type": "boolean", "description": "Optional explicit batch transaction flag. When true with transaction_id, a later failure inside the same contiguous batch transaction rolls back bounded file/database side effects recorded since the transaction began."}
                    },
                    "required": ["notebook_path"]
                }
            }
        }),
        // ── Config tool: get/set CLI configuration ─────────────────────────────────
        json!({
            "type": "function",
            "function": {
                "name": "config",
                "description": "Get or set astra CLI configuration. Read current settings or modify preferences like model, theme, output limits.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "setting": {
                            "type": "string",
                            "description": "Setting key. Available: 'model', 'api_key', 'output_limit', 'sandbox_mode', 'auto_approve', 'theme'. Use 'list' to see all settings."
                        },
                        "value": {
                            "type": "string",
                            "description": "New value. Omit to read current value."
                        }
                    },
                    "required": ["setting"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "brief",
                "description": "Return a compact session summary for context compression. Includes effective project root, worktree state, git change counts, in-memory tasks, recent file reads, and current output budget usage.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "focus": {
                            "type": "string",
                            "enum": ["all", "git", "tasks", "files", "session"],
                            "description": "Primary focus area. Default: all"
                        },
                        "max_items": {
                            "type": "integer",
                            "description": "Maximum number of tasks/files to include per section. Default 5"
                        }
                    }
                }
            }
        }),
        // ── Shared context tools for cross-agent knowledge sharing ─────────────────
        json!({
            "type": "function",
            "function": {
                "name": "share_context",
                "description": "Share knowledge with other agents in the same session. Use this to communicate findings, patterns, or insights that sibling agents might need. Knowledge is stored with a semantic key for retrieval.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "key": {
                            "type": "string",
                            "description": "Semantic key for the knowledge (e.g., 'auth/jwt-config', 'db/schema-version', 'api/endpoints'). Use slashes for namespacing."
                        },
                        "value": {
                            "description": "The knowledge to share (any JSON-serializable value)"
                        },
                        "category": {
                            "type": "string",
                            "enum": ["code_pattern", "dependency", "architecture", "security", "performance", "documentation", "custom"],
                            "description": "Category of knowledge for filtering. Default: custom"
                        }
                    },
                    "required": ["key", "value"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "query_context",
                "description": "Query knowledge shared by other agents or this agent. Use to check if information has already been discovered by sibling agents to avoid redundant work.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "key": {
                            "type": "string",
                            "description": "Exact key to lookup (e.g., 'auth/jwt-config')"
                        },
                        "prefix": {
                            "type": "string",
                            "description": "Key prefix to search (e.g., 'auth/' returns all auth-related knowledge). Cannot be used with 'key'."
                        },
                        "list_keys": {
                            "type": "boolean",
                            "description": "If true, returns only the list of available keys without values. Useful for discovering what knowledge exists."
                        },
                        "include_findings": {
                            "type": "boolean",
                            "description": "If true, also returns structured findings from completed agents."
                        }
                    }
                }
            }
        }),
    ]
}
