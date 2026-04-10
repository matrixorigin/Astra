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
                "description": "Execute a shell command in the project root. Use for builds, tests, installs, and other CLI tasks. FORBIDDEN for git inspection — NEVER use bash for `git status`, `git diff`, `git log`, `git show`, or similar git commands. Use git_status, git_diff, git_log, git_show tools instead. Can run curl, GitHub API, etc. Timeout varies (5-30s); override with timeout.",
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
                        "outline": {"type": "boolean", "description": "If true, return only function/class/struct/trait signatures with line numbers instead of full content. Ideal for understanding file structure."}
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
                "description": "Replace an exact string in a file. old_str must match exactly (including whitespace). On mismatch, shows closest matches with line numbers so you can fix and retry. Set dry_run=true to preview the diff without applying.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "File path relative to project root"},
                        "old_str": {"type": "string", "description": "Exact string to replace"},
                        "new_str": {"type": "string", "description": "Replacement string"},
                        "dry_run": {"type": "boolean", "description": "If true, show unified diff without applying changes (default: false)"},
                        "replace_all": {"type": "boolean", "description": "If true, replace ALL occurrences of old_str (default: false, requires unique match)"}
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
                        "path": {"type": "string", "description": "File path relative to project root"}
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
                        "dry_run": {"type": "boolean", "description": "If true, show unified diff without applying (default: false)"}
                    },
                    "required": ["path", "edits"]
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
                "description": "Show git diff of working tree / index vs HEAD (or two-tree diff when `ref` is set without `path`). For commit review use git_show. Use stat_only:true for `git diff --stat`-style per-file line counts without full hunks — prefer this over bash.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "ref": {"type": "string", "description": "Git ref to diff against (optional). With `path`: diff working tree vs this ref for that path. Without `path`: diff between this ref and HEAD (two-tree)."},
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
                        "all": {"type": "boolean", "description": "Stage all tracked changes (like git commit -a)"}
                    },
                    "required": ["message"]
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
                        "action": {"type": "string", "enum": ["push", "pop", "list", "drop"], "description": "Stash operation"},
                        "message": {"type": "string", "description": "Description for push (optional)"},
                        "index": {"type": "integer", "description": "Stash index for pop/drop (default 0)"}
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
                        "ref": {"type": "string", "description": "Restore from specific commit/ref (default: HEAD)"}
                    },
                    "required": ["path"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "git_worktree",
                "description": "Manage git worktrees for isolated parallel work. Actions: enter (create worktree and switch session into it), exit (leave worktree and restore original directory), add (create without switching), list (show all), remove (cleanup). Use 'enter' for session-scoped isolation; the session directory changes to the worktree. Use 'exit' with action='keep' to preserve work, or action='remove' to discard.",
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
                        "dry_run": {"type": "boolean", "description": "Preview changes without applying (default: true). Set false to apply."}
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
                "description": "Execute a SQL query against MatrixOne database. Returns formatted table results (truncated to ~20KB). Destructive operations (DELETE, DROP, TRUNCATE) are blocked by default — pass allow_destructive=true to confirm. Use for data exploration, schema inspection, and analytics queries.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "sql": {"type": "string", "description": "SQL query to execute"},
                        "database": {"type": "string", "description": "Database name (default: from MATRIXONE_DATABASE env)"}
                    },
                    "required": ["sql"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "mo_snapshot",
                "description": "Manage MatrixOne data snapshots for point-in-time recovery and experiment isolation. Actions: create (name a checkpoint), list (show all), drop (remove), restore (rollback to snapshot).",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "action": {"type": "string", "enum": ["create", "list", "drop", "restore"], "description": "Snapshot operation"},
                        "name": {"type": "string", "description": "Snapshot name (required for create/drop/restore)"}
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
        json!({
            "type": "function",
            "function": {
                "name": "run_chain",
                "description": "Execute a multi-step tool chain. Each step runs a tool and passes its output to the next step via variable substitution ($prev for previous output, $step.{key} for named step output, $input.{key} for original input). Stops on first error. Use for complex multi-tool workflows like: find files → read contents → analyze.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "name": {"type": "string", "description": "Chain name for logging"},
                        "description": {"type": "string", "description": "What this chain does"},
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
                "description": "Create a structured task for tracking complex multi-step work. Use proactively when: (1) task requires 3+ distinct steps, (2) plan mode is active, (3) user provides multiple tasks. Skip for single trivial tasks.",
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
                "description": "Update a task's status or progress. Always mark task as 'in_progress' BEFORE starting work, then 'completed' when done.",
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
                "description": "Stop/cancel a running task. Use when a task needs to be aborted before completion.",
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
        super::agent_messaging::send_message_schema(),
        // ── spawn_agent: Dynamic agent spawning ─────────────────────────────────
        astra_runtime::orchestration::spawn_agent_schema(),
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
                "description": "Interact with Language Server Protocol for code intelligence. Unified interface for definition, references, hover, symbols, and call hierarchy. More powerful than individual tools with consistent interface.",
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
                                "rename",
                                "diagnostics"
                            ],
                            "description": "LSP operation to perform"
                        },
                        "file": {
                            "type": "string",
                            "description": "File path (required for most operations)"
                        },
                        "line": {
                            "type": "integer",
                            "description": "Line number (1-based). Required for position-based operations."
                        },
                        "column": {
                            "type": "integer",
                            "description": "Column/character offset (1-based). Required for position-based operations."
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
                            "description": "For rename, preview by default. Set false only when falling back to symbol-based application."
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
                        }
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
