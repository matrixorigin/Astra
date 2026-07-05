use serde::{Deserialize, Serialize};

use crate::capability::Capability;

/// Intent type for catalog diagnostics and tool discovery metadata.
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

/// Data source scope for catalog diagnostics and capability filtering.
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

/// Static metadata for a tool — used for catalog diagnostics and discovery, never sent to LLM.
#[derive(Debug, Clone)]
pub struct ToolMeta {
    /// Tool function name (e.g. "bash", "github")
    pub name: &'static str,
    /// Short description for deferred discovery metadata.
    pub description: &'static str,
    /// Trigger phrases — search hints for `tool_search`.
    pub triggers: &'static [&'static str],
    /// Intent classification tags
    pub intents: &'static [IntentType],
    /// Data source scope
    pub scope: Scope,
    /// Runtime capabilities required before this tool can be advertised.
    pub requires: &'static [Capability],
    /// Calls that may be routed to schema/shape validation without an active
    /// executor for `requires`. This is for validation-only action shapes,
    /// not for executing capability-gated work.
    pub binding_validation: RuntimeBindingValidation,
    /// Estimated token cost of the full JSON schema (~JSON bytes / 4)
    pub schema_tokens: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeBindingValidation {
    None,
    ActionAllowlist(&'static [&'static str]),
}

impl RuntimeBindingValidation {
    pub fn allows_action(self, action: Option<&str>) -> bool {
        match self {
            RuntimeBindingValidation::None => false,
            RuntimeBindingValidation::ActionAllowlist(actions) => {
                actions.contains(&action.unwrap_or(""))
            }
        }
    }
}

// ─── Tool catalog ───────────────────────────────────────────────────────────

/// Complete catalog of all tools with their metadata.
///
/// Cross-language coverage comes from rich multilingual triggers — each tool
/// includes Chinese and English synonyms, common abbreviations, and semantic
/// associations. This eliminates the need for embedding models while providing
/// deterministic, debuggable, zero-latency matching.
pub static TOOL_CATALOG: &[ToolMeta] = &[
    // ── Built-in tools ──────────────────────────────────────────────
    //
    ToolMeta {
        name: "bash",
        description: "Execute shell commands for builds, tests, installs, git, CLI tasks",
        triggers: &[
            "run", "execute", "build", "test", "install", "command", "shell", "script", "compile",
            "运行", "执行", "编译", "测试", "安装", "命令", "脚本",
        ],
        intents: &[IntentType::CodeEdit, IntentType::CodeRead, IntentType::Git],
        scope: Scope::Local,
        requires: &[],
        binding_validation: RuntimeBindingValidation::None,
        schema_tokens: 35,
    },
    ToolMeta {
        name: "publish_artifact",
        description: "Publish a generated workspace or /tmp file for web preview and download",
        triggers: &[
            "publish_artifact",
            "publish artifact",
            "download file",
            "preview file",
            "发布文件",
            "下载文件",
            "预览文件",
        ],
        intents: &[IntentType::CodeEdit],
        scope: Scope::Local,
        requires: &[],
        binding_validation: RuntimeBindingValidation::None,
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
        intents: &[IntentType::CodeRead],
        scope: Scope::Local,
        requires: &[],
        binding_validation: RuntimeBindingValidation::None,
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
        intents: &[IntentType::CodeEdit],
        scope: Scope::Local,
        requires: &[],
        binding_validation: RuntimeBindingValidation::None,
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
        intents: &[IntentType::CodeEdit],
        scope: Scope::Local,
        requires: &[],
        binding_validation: RuntimeBindingValidation::None,
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
        intents: &[IntentType::CodeRead],
        scope: Scope::Local,
        requires: &[],
        binding_validation: RuntimeBindingValidation::None,
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
        intents: &[IntentType::CodeRead],
        scope: Scope::Local,
        requires: &[],
        binding_validation: RuntimeBindingValidation::None,
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
        intents: &[IntentType::CodeRead],
        scope: Scope::Local,
        requires: &[],
        binding_validation: RuntimeBindingValidation::None,
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
        intents: &[IntentType::Introspect],
        scope: Scope::Local,
        requires: &[],
        binding_validation: RuntimeBindingValidation::None,
        schema_tokens: 20,
    },
    ToolMeta {
        name: "reflect",
        description: "Inspect persisted session observations through topic/facet reflection views",
        triggers: &[
            "reflect",
            "reflection",
            "session diagnosis",
            "root cause",
            "why failed",
            "trace analysis",
            "反思",
            "诊断",
            "失败原因",
            "因果链",
            "会话分析",
        ],
        intents: &[IntentType::Introspect, IntentType::CodeRead],
        scope: Scope::Local,
        requires: &[],
        binding_validation: RuntimeBindingValidation::None,
        schema_tokens: 45,
    },
    ToolMeta {
        name: "tool_search",
        description: "Search and activate deferred tools. `select:NAME` queues the tool schema \
             for the next request and returns compact callable shape.",
        triggers: &[
            "tool_search",
            "find tool",
            "which tool",
            "available tools",
            "搜工具",
            "查工具",
        ],
        intents: &[IntentType::CodeRead],
        scope: Scope::Local,
        requires: &[],
        binding_validation: RuntimeBindingValidation::None,
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
        intents: &[IntentType::CodeRead, IntentType::CodeEdit],
        scope: Scope::Local,
        requires: &[Capability::LSPServer],
        binding_validation: RuntimeBindingValidation::None,
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
        intents: &[IntentType::Git],
        scope: Scope::LocalGit,
        requires: &[],
        binding_validation: RuntimeBindingValidation::None,
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
        intents: &[IntentType::GitHub],
        scope: Scope::External,
        requires: &[Capability::GitHubAuth],
        binding_validation: RuntimeBindingValidation::None,
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
        intents: &[IntentType::CodeRead],
        scope: Scope::External,
        requires: &[],
        binding_validation: RuntimeBindingValidation::None,
        schema_tokens: 25,
    },
    // ToolSpec marks memory as always-load: intrinsic memory capability. The
    // model must always be able to store and retrieve memories regardless of
    // query content. Without this, implicit preferences and background
    // extraction have no stable path.
    ToolMeta {
        name: "memory",
        description: "Memory operations: store, retrieve, purge, correct, profile, search, feedback. Pass action parameter.",
        triggers: &[
            "memory", "remember", "recall", "forget", "store", "retrieve", "记忆", "记住", "回忆",
            "存储",
        ],
        intents: &[IntentType::Memory],
        scope: Scope::CrossSession,
        requires: &[Capability::MemoryService],
        binding_validation: RuntimeBindingValidation::None,
        schema_tokens: 40,
    },
    ToolMeta {
        name: "session",
        description: "Session lifecycle and history operations: config, sleep, history_page, history_search, history_around.",
        triggers: &[
            "config",
            "adjust",
            "rollback",
            "sleep",
            "history",
            "session history",
            "配置",
            "历史",
        ],
        intents: &[IntentType::Introspect],
        scope: Scope::Local,
        requires: &[],
        binding_validation: RuntimeBindingValidation::None,
        schema_tokens: 60,
    },
    ToolMeta {
        name: "compress_context",
        description: "Request manual context compression for this session",
        triggers: &[
            "compress_context",
            "compact context",
            "compress context",
            "compact history",
            "压缩上下文",
            "压缩历史",
        ],
        intents: &[IntentType::Introspect],
        scope: Scope::Local,
        requires: &[],
        binding_validation: RuntimeBindingValidation::None,
        schema_tokens: 25,
    },
    ToolMeta {
        name: "rollback_session_state",
        description: "List or restore session-state mutations",
        triggers: &[
            "rollback_session_state",
            "rollback session",
            "restore session state",
            "undo preference",
            "回滚会话",
            "恢复会话状态",
        ],
        intents: &[IntentType::Introspect],
        scope: Scope::Local,
        requires: &[],
        binding_validation: RuntimeBindingValidation::None,
        schema_tokens: 35,
    },
    ToolMeta {
        name: "mo_query",
        description: "Run MatrixOne SQL with pre-state rollback snapshot support",
        triggers: &[
            "mo_query",
            "matrixone query",
            "run sql",
            "sql query",
            "数据库查询",
            "执行 SQL",
        ],
        intents: &[IntentType::Database],
        scope: Scope::External,
        requires: &[Capability::Database],
        binding_validation: RuntimeBindingValidation::None,
        schema_tokens: 35,
    },
    ToolMeta {
        name: "rollback_database_snapshots",
        description: "List or restore MatrixOne rollback snapshots",
        triggers: &[
            "rollback_database_snapshots",
            "database rollback",
            "restore database snapshot",
            "matrixone rollback",
            "数据库回滚",
            "恢复数据库快照",
        ],
        intents: &[IntentType::Database],
        scope: Scope::External,
        requires: &[Capability::Database],
        binding_validation: RuntimeBindingValidation::None,
        schema_tokens: 35,
    },
    ToolMeta {
        name: "agent",
        description: "Multi-agent operations: spawn one sub-agent, collect one background result, send messages, or execute a tool chain.",
        triggers: &["agent", "spawn", "chain", "orchestrate", "代理"],
        intents: &[IntentType::CodeEdit],
        scope: Scope::External,
        requires: &[Capability::AgentSpawner],
        binding_validation: RuntimeBindingValidation::ActionAllowlist(&["", "run_chain"]),
        schema_tokens: 40,
    },
    ToolMeta {
        name: "agent_fanout",
        description: "Atomic fixed-size parallel sub-agent fan-out: start a group, collect group results, or stop one slot.",
        triggers: &[
            "fanout",
            "parallel agents",
            "review agents",
            "multi-agent",
            // Chinese coverage: explicit fan-out review phrasings.
            "多agents",
            "多 agents",
            "多个 agents",
            "多视角 review",
            "多角度 review",
            "并行代理",
            "并行审查",
            "并行 review",
            "多角度审查",
            "多视角审查",
            "同时审查",
        ],
        intents: &[IntentType::CodeEdit],
        scope: Scope::External,
        requires: &[Capability::AgentSpawner],
        binding_validation: RuntimeBindingValidation::None,
        schema_tokens: 45,
    },
    ToolMeta {
        name: "task_output",
        description: "Read output and status for a specific typed background task by task_id.",
        triggers: &[
            "task output",
            "background output",
            "shell output",
            "bg output",
        ],
        intents: &[IntentType::CodeEdit],
        scope: Scope::Local,
        requires: &[Capability::LocalBackgroundTasks],
        binding_validation: RuntimeBindingValidation::None,
        schema_tokens: 35,
    },
    ToolMeta {
        name: "task_stop",
        description: "Stop a specific running typed background task by task_id.",
        triggers: &[
            "task stop",
            "stop background",
            "kill background",
            "cancel background",
        ],
        intents: &[IntentType::CodeEdit],
        scope: Scope::Local,
        requires: &[Capability::LocalBackgroundTasks],
        binding_validation: RuntimeBindingValidation::None,
        schema_tokens: 25,
    },
    ToolMeta {
        name: "task_list",
        description: "List typed background tasks for this session.",
        triggers: &[
            "task list",
            "background list",
            "list background",
            "bg tasks",
        ],
        intents: &[IntentType::CodeEdit],
        scope: Scope::Local,
        requires: &[Capability::LocalBackgroundTasks],
        binding_validation: RuntimeBindingValidation::None,
        schema_tokens: 20,
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
        intents: &[IntentType::CodeRead],
        scope: Scope::Local,
        requires: &[],
        binding_validation: RuntimeBindingValidation::None,
        schema_tokens: 25,
    },
    ToolMeta {
        name: "powershell",
        description: "Execute PowerShell commands for Windows shell tasks and cross-platform automation",
        triggers: &["powershell", "pwsh", "ps1", "windows", "PowerShell"],
        intents: &[IntentType::CodeEdit, IntentType::CodeRead],
        scope: Scope::Local,
        requires: &[],
        binding_validation: RuntimeBindingValidation::None,
        schema_tokens: 15,
    },
    ToolMeta {
        name: "run_script",
        description: "Execute a structured script via sandbox RPC transport (Unix-only)",
        triggers: &["run_script", "script", "sandbox", "rpc"],
        intents: &[IntentType::CodeEdit],
        scope: Scope::Local,
        requires: &[],
        binding_validation: RuntimeBindingValidation::None,
        schema_tokens: 40,
    },
    ToolMeta {
        name: "skill",
        description: "Execute a discovered skill by name. Skills wrap reusable workflows.",
        triggers: &["skill", "workflow", "技能"],
        intents: &[IntentType::CodeRead],
        scope: Scope::Local,
        requires: &[Capability::SkillsCatalog],
        binding_validation: RuntimeBindingValidation::None,
        schema_tokens: 30,
    },
    ToolMeta {
        name: "enter_plan_mode",
        description: "Switch the runtime into plan-authoring mode. Server-owned state machine.",
        triggers: &["plan", "enter plan mode"],
        intents: &[IntentType::CodeEdit],
        scope: Scope::External,
        requires: &[Capability::PlanLifecycle],
        binding_validation: RuntimeBindingValidation::None,
        schema_tokens: 15,
    },
    ToolMeta {
        name: "exit_plan_mode",
        description: "Submit the authored plan for user review and exit plan-authoring mode.",
        triggers: &["exit plan", "submit plan"],
        intents: &[IntentType::CodeEdit],
        scope: Scope::External,
        requires: &[Capability::PlanLifecycle],
        binding_validation: RuntimeBindingValidation::None,
        schema_tokens: 20,
    },
];

/// Look up the static [`ToolMeta`] for a tool by name.
///
/// This is the single declarative source of truth for "what does this tool
/// require at runtime?" Callers (admission checks, activation recording,
/// capability introspection) must use this instead of hardcoding tool-name
/// match arms — adding a new tool that needs `AgentSpawner` is then just
/// setting `requires: &[Capability::AgentSpawner]` in the catalog entry.
pub fn tool_meta(name: &str) -> Option<&'static ToolMeta> {
    TOOL_CATALOG.iter().find(|t| t.name == name)
}

/// Return whether a tool call may perform validation without an active
/// executor binding for its declared capabilities.
pub fn tool_allows_validation_without_runtime_binding(name: &str, action: Option<&str>) -> bool {
    tool_meta(name).is_some_and(|meta| meta.binding_validation.allows_action(action))
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
        // MatrixOne exposes the canonical query tool plus rollback support;
        // agent fan-out is a dedicated tool.
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
    fn catalog_includes_top_level_session_state_tools() {
        for name in ["introspect", "compress_context", "rollback_session_state"] {
            let tool = TOOL_CATALOG
                .iter()
                .find(|tool| tool.name == name)
                .unwrap_or_else(|| panic!("{name} must exist in catalog"));
            assert!(
                tool.intents.contains(&IntentType::Introspect),
                "{name} should be modeled as a session-state/introspection control tool"
            );
        }
    }

    #[test]
    fn capability_gated_tools_declare_requires() {
        let cases = [
            ("agent", Capability::AgentSpawner),
            ("agent_fanout", Capability::AgentSpawner),
            ("memory", Capability::MemoryService),
            ("mo_query", Capability::Database),
            ("rollback_database_snapshots", Capability::Database),
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
    fn unbound_validation_policy_is_declared_in_tool_metadata() {
        assert!(tool_allows_validation_without_runtime_binding(
            "agent",
            Some("run_chain")
        ));
        assert!(!tool_allows_validation_without_runtime_binding(
            "agent",
            Some("delegate")
        ));
        assert!(tool_allows_validation_without_runtime_binding(
            "agent", None
        ));
        assert!(!tool_allows_validation_without_runtime_binding(
            "agent",
            Some("spawn")
        ));
        assert!(!tool_allows_validation_without_runtime_binding(
            "agent_fanout",
            Some("start")
        ));
    }

    #[test]
    fn background_task_tool_descriptions_are_not_shell_only() {
        for name in ["task_output", "task_stop", "task_list"] {
            let tool = TOOL_CATALOG
                .iter()
                .find(|tool| tool.name == name)
                .unwrap_or_else(|| panic!("{name} must exist in catalog"));
            assert!(
                tool.description.contains("background task"),
                "{name} should describe typed background tasks: {}",
                tool.description
            );
            assert!(
                !tool.description.contains("background shell"),
                "{name} must not narrow the task model to shells: {}",
                tool.description
            );
        }
    }
}
