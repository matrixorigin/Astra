use serde::{Deserialize, Serialize};

use crate::capability::Capability;

/// Intent type for tag-based pre-filtering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntentType {
    /// Code editing, file manipulation, builds
    CodeEdit,
    /// Read-only inspection, search, navigation
    CodeRead,
    /// Git operations (status, diff, log)
    Git,
    /// GitHub API (PRs, issues, CI)
    GitHub,
    /// Memory storage/retrieval
    Memory,
    /// Agent introspection, reflection
    Introspect,
    /// Database operations (MatrixOne query, snapshot, branch)
    Database,
}

impl IntentType {
    /// Get the snake_case string representation of this intent type.
    pub fn as_str(&self) -> &'static str {
        match self {
            IntentType::CodeEdit => "code_edit",
            IntentType::CodeRead => "code_read",
            IntentType::Git => "git",
            IntentType::GitHub => "github",
            IntentType::Memory => "memory",
            IntentType::Introspect => "introspect",
            IntentType::Database => "database",
        }
    }
}

/// Data source scope for tag-based pre-filtering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    /// Local filesystem operations
    Local,
    /// Local git repository
    LocalGit,
    /// External API calls (GitHub, etc.)
    External,
    /// Cross-session persistent store (memory)
    CrossSession,
}

/// Static metadata for a tool — used for selection, never sent to LLM.
#[derive(Debug, Clone)]
pub struct ToolMeta {
    /// Tool function name (e.g. "bash", "github_list_prs")
    pub name: &'static str,
    /// Short description for embedding index
    pub description: &'static str,
    /// Trigger phrases — additional semantic signals for retrieval
    pub triggers: &'static [&'static str],
    /// Whether this tool is always included (no selection needed)
    pub pinned: bool,
    /// Intent classification tags
    pub intents: &'static [IntentType],
    /// Data source scope
    pub scope: Scope,
    /// Runtime capabilities required before this tool can be advertised.
    pub requires: &'static [Capability],
    /// Estimated token cost of the full JSON schema (~JSON bytes / 4)
    pub schema_tokens: u32,
}

// ─── Tool catalog ───────────────────────────────────────────────────────────

/// Complete catalog of all tools with their metadata.
///
/// Cross-language coverage comes from rich multilingual triggers — each tool
/// includes Chinese and English synonyms, common abbreviations, and semantic
/// associations. This eliminates the need for embedding models while providing
/// deterministic, debuggable, zero-latency matching.
pub static TOOL_CATALOG: &[ToolMeta] = &[
    // ── Pinned tools (always available) ─────────────────────────────
    ToolMeta {
        name: "bash",
        description: "Execute shell commands for builds, tests, installs, git, CLI tasks",
        triggers: &[
            "run", "execute", "build", "test", "install", "command", "shell", "script", "compile",
            "运行", "执行", "编译", "测试", "安装", "命令", "脚本",
        ],
        pinned: true,
        intents: &[IntentType::CodeEdit, IntentType::CodeRead, IntentType::Git],
        scope: Scope::Local,
        requires: &[],
        schema_tokens: 35,
    },
    ToolMeta {
        name: "read_file",
        description: "Read file contents with optional line range",
        triggers: &[
            "read",
            "view",
            "show",
            "inspect",
            "look at",
            "open",
            "cat",
            "display",
            "cat file",
            "type file",
            "contents of",
            "查看",
            "读取",
            "打开",
            "看看",
            "看一下",
            "文件内容",
            "查看文件",
        ],
        pinned: true,
        intents: &[IntentType::CodeRead],
        scope: Scope::Local,
        requires: &[],
        schema_tokens: 35,
    },
    ToolMeta {
        name: "write_file",
        description: "Create, overwrite, or delete a file; writes require path+content, deletes use delete=true",
        triggers: &[
            "write",
            "create file",
            "save file",
            "output",
            "generate file",
            "write to",
            "new file",
            "make file",
            "touch",
            "export to",
            "dump to",
            "write out",
            "创建文件",
            "写入",
            "保存文件",
            "生成文件",
            "写文件",
            "新建文件",
            "输出到文件",
            "导出文件",
        ],
        // Pinned: paired with str_replace/read_file as the basic edit triad.
        pinned: true,
        intents: &[IntentType::CodeEdit],
        scope: Scope::Local,
        requires: &[],
        schema_tokens: 25,
    },
    ToolMeta {
        name: "str_replace",
        description: "Replace text in files with exact string matching",
        triggers: &[
            "replace",
            "edit",
            "modify",
            "update",
            "fix",
            "refactor",
            "rename",
            "edit file",
            "modify file",
            "change text",
            "替换",
            "修改",
            "编辑",
            "改代码",
            "改文件",
            "重构",
            "修改文件",
            "替换文本",
            "更改",
            // Note: 重命名 is intentionally not here — LSP rename is preferred for semantic renames
        ],
        pinned: true,
        intents: &[IntentType::CodeEdit],
        scope: Scope::Local,
        requires: &[],
        schema_tokens: 40,
    },
    ToolMeta {
        name: "list_dir",
        description: "List directory structure and files",
        triggers: &[
            "list directory",
            "directory",
            "files",
            "list files",
            "tree",
            "structure",
            "folder",
            "dir",
            "what files",
            "show files",
            "browse",
            "contents",
            "目录",
            "文件列表",
            "文件夹",
            "结构",
            "列出文件",
            "看看有什么文件",
            "文件结构",
            "目录结构",
        ],
        pinned: true,
        intents: &[IntentType::CodeRead],
        scope: Scope::Local,
        requires: &[],
        schema_tokens: 25,
    },
    ToolMeta {
        name: "grep",
        description: "Search file contents with regex patterns",
        triggers: &[
            "grep",
            "regex",
            "find in files",
            "text search",
            "search code",
            "搜索代码",
            "代码搜索",
            "全文搜索",
        ],
        // Pinned: near-universal for code navigation — including in the static
        // tool prefix keeps prompt cache stable across turns.
        pinned: true,
        intents: &[IntentType::CodeRead],
        scope: Scope::Local,
        requires: &[],
        schema_tokens: 40,
    },
    ToolMeta {
        name: "glob",
        description: "Find files by name pattern",
        triggers: &[
            "find files",
            "glob",
            "locate",
            "file pattern",
            "find by name",
            "file matching",
            "locate files",
            "by extension",
            "*.py",
            "*.rs",
            "*.ts",
            "找文件",
            "文件名",
            "定位文件",
            "查找文件",
            "名称匹配",
            "文件模式",
            "扩展名",
        ],
        // Pinned: partner to grep for locating files before reading them.
        pinned: true,
        intents: &[IntentType::CodeRead],
        scope: Scope::Local,
        requires: &[],
        schema_tokens: 25,
    },
    ToolMeta {
        name: "introspect",
        description: "Query own runtime state: pressure, cache, tool health, alerts",
        triggers: &[
            "introspect",
            "self-check",
            "status",
            "health",
            "diagnostics",
            "自省",
            "自检",
            "状态",
            "健康度",
        ],
        pinned: true,
        intents: &[IntentType::CodeRead],
        scope: Scope::Local,
        requires: &[],
        schema_tokens: 20,
    },
    ToolMeta {
        name: "tool_search",
        description: "Search and activate deferred tools. `select:NAME` returns the full schema \
             for a deferred tool so the LLM can invoke it on the next turn.",
        triggers: &[
            "tool_search",
            "find tool",
            "which tool",
            "available tools",
            "搜工具",
            "查工具",
        ],
        // Pinned: activation primitive for the deferred tool layer. Must always
        // be in tools[] so the model can reach any deferred tool.
        pinned: true,
        intents: &[IntentType::CodeRead],
        scope: Scope::Local,
        requires: &[],
        schema_tokens: 25,
    },
    // ── Dynamic tools (selected per-request) ────────────────────────
    ToolMeta {
        name: "lsp",
        description: "Unified Language Server Protocol code intelligence and editor actions: definitions, references, hover, call/type hierarchy, rename, code actions, diagnostics, formatting",
        triggers: &[
            "lsp",
            "language server",
            "go to definition",
            "definition",
            "find references",
            "references",
            "hover",
            "implementation",
            "type hierarchy",
            "supertypes",
            "subtypes",
            "inheritance",
            "type definition",
            "declaration",
            "rename symbol",
            "prepare rename",
            "code action",
            "diagnostics",
            "format document",
            "format range",
            "selection range",
            "document highlight",
            "document link",
            "linked editing",
            "语义跳转",
            "定义",
            "引用",
            "悬停",
            "实现",
            "类型层次",
            "父类型",
            "子类型",
            "类型定义",
            "声明",
            "重命名",
            "代码动作",
            "诊断",
            "格式化",
            "高亮",
            "选择范围",
            "文档链接",
            "联动编辑",
        ],
        pinned: true,
        intents: &[IntentType::CodeRead, IntentType::CodeEdit],
        scope: Scope::Local,
        requires: &[Capability::LSPServer],
        schema_tokens: 90,
    },
    ToolMeta {
        name: "git",
        description: "Git operations: status, diff, log, show, blame, file_history, log_search, contributors, commit, revert_commit, stash. Pass action as first parameter.",
        triggers: &[
            "git",
            "commit",
            "diff",
            "blame",
            "history",
            "log",
            "status",
            "branch",
            "stash",
            "contributors",
            "revert",
            "提交",
            "版本",
            "差异",
            "日志",
            "分支",
            "历史",
        ],
        pinned: true,
        intents: &[IntentType::Git],
        scope: Scope::LocalGit,
        requires: &[],
        schema_tokens: 50,
    },
    ToolMeta {
        name: "github",
        description: "GitHub operations: list_prs, get_pr, ci_status, repo_stats, list_issues, get_issue, create_issue. Pass action parameter.",
        triggers: &[
            "github",
            "pull request",
            "PR",
            "issue",
            "CI",
            "repo",
            "merge",
            "review",
        ],
        pinned: true,
        intents: &[IntentType::GitHub],
        scope: Scope::External,
        requires: &[Capability::GitHubAuth],
        schema_tokens: 50,
    },
    ToolMeta {
        name: "web_fetch",
        description: "Fetch URL content — web pages, APIs, documentation",
        triggers: &[
            "fetch",
            "url",
            "web",
            "http",
            "download",
            "api call",
            "web page",
            "read url",
            "curl",
            "link",
            "open url",
            "get webpage",
            "网页",
            "网址",
            "获取网页",
            "访问链接",
            "链接",
            "打开链接",
            "获取页面",
            "打开网址",
        ],
        pinned: true,
        intents: &[IntentType::CodeRead],
        scope: Scope::External,
        requires: &[],
        schema_tokens: 25,
    },
    // memory_store is pinned — intrinsic memory capability. The model must
    // always be able to store memories regardless of query content. Without
    // this, implicit preferences and background extraction have no way to
    // persist information.
    ToolMeta {
        name: "memory",
        description: "Memory operations: store, retrieve, purge, correct, profile, search, feedback. Pass action parameter.",
        triggers: &[
            "memory", "remember", "recall", "forget", "store", "retrieve", "记忆", "记住", "回忆",
            "存储",
        ],
        pinned: true,
        intents: &[IntentType::Memory],
        scope: Scope::CrossSession,
        requires: &[Capability::MemoryService],
        schema_tokens: 40,
    },
    ToolMeta {
        name: "session",
        description: "Session state and lifecycle operations: config, prioritize, deprioritize, compact, rollback_edits, ask_user, sleep, timeline, summary, history, suppress_memory(memory_id), unsuppress_memory(memory_id), list_suppressed, release_context(tool_call_id|string[]), list_released.",
        triggers: &[
            "config",
            "adjust",
            "prioritize",
            "deprioritize",
            "goal",
            "compact",
            "plan",
            "rollback",
            "ask",
            "sleep",
            "search tools",
            "suppress",
            "unsuppress",
            "release",
            "压缩",
            "目标",
            "配置",
            "计划",
        ],
        pinned: true,
        intents: &[IntentType::Introspect],
        scope: Scope::Local,
        requires: &[],
        schema_tokens: 60,
    },
    ToolMeta {
        name: "mo",
        description: "MatrixOne database operations: query, snapshot, branch. Execute SQL and manage DB state.",
        triggers: &[
            "sql",
            "query",
            "database",
            "matrixone",
            "snapshot",
            "branch",
            "数据库",
            "查询",
        ],
        pinned: true,
        intents: &[IntentType::CodeEdit],
        scope: Scope::External,
        requires: &[Capability::Database],
        schema_tokens: 40,
    },
    ToolMeta {
        name: "agent",
        description: "Multi-agent operations: spawn (create sub-agent), get_result (collect background result), run_chain (execute tool chain). For complex tasks requiring sub-agents. Fan out N agents in parallel by emitting N spawn calls in one assistant message with run_in_background:true.",
        triggers: &["agent", "spawn", "chain", "orchestrate", "代理"],
        pinned: true,
        intents: &[IntentType::CodeEdit],
        scope: Scope::External,
        requires: &[Capability::AgentSpawner],
        schema_tokens: 40,
    },
    ToolMeta {
        name: "symbols",
        description: "Extract function/class/struct signatures from a file using tree-sitter",
        triggers: &[
            "symbols",
            "functions",
            "classes",
            "signatures",
            "outline",
            "符号",
            "函数列表",
            "类列表",
        ],
        pinned: true,
        intents: &[IntentType::CodeRead],
        scope: Scope::Local,
        requires: &[],
        schema_tokens: 25,
    },
    ToolMeta {
        name: "powershell",
        description: "Execute PowerShell commands for Windows shell tasks and cross-platform automation",
        triggers: &["powershell", "pwsh", "ps1", "windows", "PowerShell"],
        pinned: true,
        intents: &[IntentType::CodeEdit, IntentType::CodeRead],
        scope: Scope::Local,
        requires: &[],
        schema_tokens: 15,
    },
    ToolMeta {
        name: "run_script",
        description: "Execute a structured script via sandbox RPC transport (Unix-only)",
        triggers: &["run_script", "script", "sandbox", "rpc"],
        pinned: true,
        intents: &[IntentType::CodeEdit],
        scope: Scope::Local,
        requires: &[],
        schema_tokens: 40,
    },
    ToolMeta {
        name: "skill",
        description: "Execute a discovered skill by name. Skills wrap reusable workflows.",
        triggers: &["skill", "workflow", "技能"],
        pinned: true,
        intents: &[IntentType::CodeRead],
        scope: Scope::Local,
        requires: &[Capability::SkillsCatalog],
        schema_tokens: 30,
    },
    ToolMeta {
        name: "enter_plan_mode",
        description: "Switch the runtime into plan-authoring mode. Server-owned state machine.",
        triggers: &["plan", "enter plan mode"],
        pinned: false,
        intents: &[IntentType::CodeEdit],
        scope: Scope::External,
        requires: &[Capability::PlanLifecycle],
        schema_tokens: 15,
    },
    ToolMeta {
        name: "exit_plan_mode",
        description: "Submit the authored plan for user review and exit plan-authoring mode.",
        triggers: &["exit plan", "submit plan"],
        pinned: false,
        intents: &[IntentType::CodeEdit],
        scope: Scope::External,
        requires: &[Capability::PlanLifecycle],
        schema_tokens: 20,
    },
];

/// Returns `true` when `name` matches a pinned tool in [`TOOL_CATALOG`].
/// Pinned tools are essential to agent operation and must never be blocked
/// by cross-session learning or pattern-library heuristics.
pub fn is_pinned_tool(name: &str) -> bool {
    TOOL_CATALOG.iter().any(|t| t.pinned && t.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    // --- IntentType ---

    #[test]
    fn intent_type_as_str_all_variants() {
        assert_eq!(IntentType::CodeEdit.as_str(), "code_edit");
        assert_eq!(IntentType::CodeRead.as_str(), "code_read");
        assert_eq!(IntentType::Git.as_str(), "git");
        assert_eq!(IntentType::GitHub.as_str(), "github");
        assert_eq!(IntentType::Memory.as_str(), "memory");
        assert_eq!(IntentType::Introspect.as_str(), "introspect");
        assert_eq!(IntentType::Database.as_str(), "database");
    }

    #[test]
    fn intent_type_serde_round_trip() {
        let variants = [
            IntentType::CodeEdit,
            IntentType::CodeRead,
            IntentType::Git,
            IntentType::GitHub,
            IntentType::Memory,
            IntentType::Introspect,
            IntentType::Database,
        ];
        for v in &variants {
            let json = serde_json::to_string(v).unwrap();
            let back: IntentType = serde_json::from_str(&json).unwrap();
            assert_eq!(*v, back);
        }
    }

    #[test]
    fn intent_type_serde_uses_snake_case() {
        let json = serde_json::to_string(&IntentType::CodeEdit).unwrap();
        assert_eq!(json, r#""code_edit""#);
        let json = serde_json::to_string(&IntentType::CodeRead).unwrap();
        assert_eq!(json, r#""code_read""#);
    }

    #[test]
    fn intent_type_deserialize_rejects_wrong_case() {
        let result = serde_json::from_str::<IntentType>(r#""CodeEdit""#);
        assert!(result.is_err());
    }

    // --- Scope ---

    #[test]
    fn scope_serde_round_trip() {
        let variants = [
            Scope::Local,
            Scope::LocalGit,
            Scope::External,
            Scope::CrossSession,
        ];
        for v in &variants {
            let json = serde_json::to_string(v).unwrap();
            let back: Scope = serde_json::from_str(&json).unwrap();
            assert_eq!(*v, back);
        }
    }

    #[test]
    fn scope_serde_uses_snake_case() {
        assert_eq!(
            serde_json::to_string(&Scope::LocalGit).unwrap(),
            r#""local_git""#
        );
        assert_eq!(
            serde_json::to_string(&Scope::CrossSession).unwrap(),
            r#""cross_session""#
        );
    }

    // --- TOOL_CATALOG validation ---

    #[test]
    fn catalog_no_duplicate_names() {
        let mut seen = HashSet::new();
        for tool in TOOL_CATALOG {
            assert!(seen.insert(tool.name), "duplicate tool name: {}", tool.name);
        }
    }

    #[test]
    fn catalog_all_tools_have_nonempty_fields() {
        for tool in TOOL_CATALOG {
            assert!(!tool.name.is_empty(), "tool has empty name");
            assert!(
                !tool.description.is_empty(),
                "tool {} has empty description",
                tool.name
            );
            assert!(
                !tool.triggers.is_empty(),
                "tool {} has no triggers",
                tool.name
            );
            assert!(
                !tool.intents.is_empty(),
                "tool {} has no intents",
                tool.name
            );
        }
    }

    #[test]
    fn catalog_pinned_tools_include_bash_and_read_file() {
        let pinned: Vec<&str> = TOOL_CATALOG
            .iter()
            .filter(|t| t.pinned)
            .map(|t| t.name)
            .collect();
        assert!(pinned.contains(&"bash"), "bash should be pinned");
        assert!(pinned.contains(&"read_file"), "read_file should be pinned");
    }

    #[test]
    fn catalog_schema_tokens_positive_for_all() {
        for tool in TOOL_CATALOG {
            assert!(
                tool.schema_tokens > 0,
                "tool {} has zero schema_tokens",
                tool.name
            );
        }
    }

    #[test]
    fn catalog_has_expected_count() {
        // Sanity check — if tools are added/removed, update this.
        // Post-consolidation: 8 git→1, 7 github→1, 5 memory→1, 5 session→1,
        // 3 mo→1, 2 agent→1 — catalog now has ~17 entries.
        assert!(
            TOOL_CATALOG.len() >= 15,
            "expected at least 15 tools, got {}",
            TOOL_CATALOG.len()
        );
    }

    #[test]
    fn catalog_no_duplicate_triggers_within_tool() {
        for tool in TOOL_CATALOG {
            let mut seen = HashSet::new();
            for trigger in tool.triggers {
                assert!(
                    seen.insert(*trigger),
                    "tool {} has duplicate trigger: {}",
                    tool.name,
                    trigger
                );
            }
        }
    }

    #[test]
    fn catalog_consolidated_memory_is_pinned() {
        let tool = TOOL_CATALOG
            .iter()
            .find(|t| t.name == "memory")
            .expect("consolidated `memory` tool must exist in catalog");
        assert!(
            tool.pinned,
            "memory must be pinned — intrinsic store/retrieve capability"
        );
    }

    #[test]
    fn capability_gated_tools_declare_requires() {
        let cases = [
            ("agent", Capability::AgentSpawner),
            ("memory", Capability::MemoryService),
            ("mo", Capability::Database),
            ("github", Capability::GitHubAuth),
            ("lsp", Capability::LSPServer),
            ("skill", Capability::SkillsCatalog),
            ("enter_plan_mode", Capability::PlanLifecycle),
            ("exit_plan_mode", Capability::PlanLifecycle),
        ];
        for (name, expected) in cases {
            let tool = TOOL_CATALOG
                .iter()
                .find(|tool| tool.name == name)
                .unwrap_or_else(|| panic!("{name} must exist in catalog"));
            assert!(
                tool.requires.contains(&expected),
                "{name} must require {expected:?}, got {:?}",
                tool.requires
            );
        }
    }

    #[test]
    fn is_pinned_tool_matches_catalog() {
        assert!(is_pinned_tool("bash"));
        assert!(is_pinned_tool("read_file"));
        assert!(is_pinned_tool("str_replace"));
        assert!(is_pinned_tool("memory"));
        assert!(is_pinned_tool("session"));
        assert!(!is_pinned_tool("nonexistent_tool"));
    }
}
