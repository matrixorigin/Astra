use serde::{Deserialize, Serialize};

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
    /// Estimated token cost of the full JSON schema (~JSON bytes / 4)
    pub schema_tokens: u32,
}

// ─── Tool catalog ───────────────────────────────────────────────────────────

/// Complete catalog of all tools with their metadata.
pub static TOOL_CATALOG: &[ToolMeta] = &[
    // ── Pinned tools (always available) ─────────────────────────────
    ToolMeta {
        name: "bash",
        description: "Execute shell commands for builds, tests, installs, git, CLI tasks",
        triggers: &[
            "run", "execute", "build", "test", "install", "command", "shell", "运行", "执行",
            "编译", "测试", "安装",
        ],
        pinned: true,
        intents: &[IntentType::CodeEdit, IntentType::CodeRead, IntentType::Git],
        scope: Scope::Local,
        schema_tokens: 30,
    },
    ToolMeta {
        name: "read_file",
        description: "Read file contents with optional line range",
        triggers: &[
            "read", "view", "show", "inspect", "look at", "open", "查看", "读取", "打开", "看看",
        ],
        pinned: true,
        intents: &[IntentType::CodeRead],
        scope: Scope::Local,
        schema_tokens: 35,
    },
    ToolMeta {
        name: "write_file",
        description: "Create or overwrite a file",
        triggers: &["write", "create", "save", "output", "创建", "写入", "保存"],
        pinned: true,
        intents: &[IntentType::CodeEdit],
        scope: Scope::Local,
        schema_tokens: 25,
    },
    ToolMeta {
        name: "str_replace",
        description: "Replace text in files with exact string matching",
        triggers: &[
            "replace", "edit", "modify", "change", "update", "fix", "替换", "修改", "编辑",
        ],
        pinned: true,
        intents: &[IntentType::CodeEdit],
        scope: Scope::Local,
        schema_tokens: 40,
    },
    ToolMeta {
        name: "list_dir",
        description: "List directory structure and files",
        triggers: &[
            "list",
            "directory",
            "files",
            "ls",
            "tree",
            "structure",
            "目录",
            "文件列表",
        ],
        pinned: true,
        intents: &[IntentType::CodeRead],
        scope: Scope::Local,
        schema_tokens: 25,
    },
    ToolMeta {
        name: "grep",
        description: "Search file contents with regex patterns",
        triggers: &[
            "search", "find", "grep", "pattern", "match", "look for", "搜索", "查找", "查",
        ],
        pinned: true,
        intents: &[IntentType::CodeRead],
        scope: Scope::Local,
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
            "找文件",
            "文件名",
        ],
        pinned: true,
        intents: &[IntentType::CodeRead],
        scope: Scope::Local,
        schema_tokens: 25,
    },
    // ── Dynamic tools (selected per-request) ────────────────────────
    ToolMeta {
        name: "git_status",
        description: "Show git working tree status",
        triggers: &[
            "git status",
            "changes",
            "modified",
            "staged",
            "uncommitted",
            "状态",
            "改了什么",
        ],
        pinned: false,
        intents: &[IntentType::Git],
        scope: Scope::LocalGit,
        schema_tokens: 15,
    },
    ToolMeta {
        name: "git_diff",
        description: "Show git diffs for files or commits",
        triggers: &["diff", "difference", "compare", "changed", "差异", "对比"],
        pinned: false,
        intents: &[IntentType::Git],
        scope: Scope::LocalGit,
        schema_tokens: 30,
    },
    ToolMeta {
        name: "git_log",
        description: "Show git commit history",
        triggers: &[
            "log",
            "history",
            "commits",
            "recent commits",
            "提交历史",
            "日志",
        ],
        pinned: false,
        intents: &[IntentType::Git],
        scope: Scope::LocalGit,
        schema_tokens: 30,
    },
    ToolMeta {
        name: "github_list_prs",
        description: "List pull requests for a GitHub repository",
        triggers: &[
            "pr",
            "pull request",
            "PRs",
            "list prs",
            "open prs",
            "merged",
            "最新的pr",
            "拉取请求",
        ],
        pinned: false,
        intents: &[IntentType::GitHub],
        scope: Scope::External,
        schema_tokens: 40,
    },
    ToolMeta {
        name: "github_get_pr",
        description: "Get details of a specific pull request",
        triggers: &[
            "pr details",
            "pull request details",
            "pr #",
            "review pr",
            "pr详情",
        ],
        pinned: false,
        intents: &[IntentType::GitHub],
        scope: Scope::External,
        schema_tokens: 30,
    },
    ToolMeta {
        name: "github_ci_status",
        description: "Get CI/CD pipeline status for a branch or PR",
        triggers: &[
            "ci",
            "pipeline",
            "build status",
            "checks",
            "actions",
            "CI状态",
            "构建状态",
        ],
        pinned: false,
        intents: &[IntentType::GitHub],
        scope: Scope::External,
        schema_tokens: 30,
    },
    ToolMeta {
        name: "github_list_issues",
        description: "List issues for a GitHub repository",
        triggers: &[
            "issues",
            "bugs",
            "list issues",
            "open issues",
            "问题列表",
            "最新的issue",
        ],
        pinned: false,
        intents: &[IntentType::GitHub],
        scope: Scope::External,
        schema_tokens: 40,
    },
    ToolMeta {
        name: "github_get_issue",
        description: "Get details of a specific issue",
        triggers: &["issue details", "issue #", "bug details", "issue详情"],
        pinned: false,
        intents: &[IntentType::GitHub],
        scope: Scope::External,
        schema_tokens: 30,
    },
    ToolMeta {
        name: "github_create_issue",
        description: "Create a new issue in a GitHub repository",
        triggers: &[
            "create issue",
            "new issue",
            "file bug",
            "report issue",
            "创建issue",
            "提issue",
        ],
        pinned: false,
        intents: &[IntentType::GitHub],
        scope: Scope::External,
        schema_tokens: 40,
    },
    // Memory tools are pinned because the system prompt always includes memory rules
    // ("STORE to memory_store when user states a preference…"). If the prompt tells the
    // LLM to use a tool, that tool must be available — otherwise the LLM describes the
    // action in text instead of calling it. Cost: +65 tokens/turn (negligible).
    ToolMeta {
        name: "memory_store",
        description: "Store information to persistent memory",
        triggers: &[
            "remember",
            "store",
            "save to memory",
            "memorize",
            "记住",
            "保存记忆",
            "存储",
        ],
        pinned: true,
        intents: &[IntentType::Memory],
        scope: Scope::CrossSession,
        schema_tokens: 35,
    },
    ToolMeta {
        name: "memory_search",
        description: "Search persistent memories for relevant information",
        triggers: &[
            "recall",
            "remember",
            "search memory",
            "what did I",
            "memories",
            "my memory",
            "记忆",
            "回忆",
            "搜索记忆",
            "记住了什么",
            "偏好",
            "哪些记忆",
        ],
        pinned: true,
        intents: &[IntentType::Memory],
        scope: Scope::CrossSession,
        schema_tokens: 30,
    },
    ToolMeta {
        name: "memory_purge",
        description: "Delete memories by query or ID",
        triggers: &[
            "forget",
            "delete memory",
            "purge",
            "remove memory",
            "删除记忆",
            "忘记",
        ],
        pinned: false,
        intents: &[IntentType::Memory],
        scope: Scope::CrossSession,
        schema_tokens: 25,
    },
    ToolMeta {
        name: "get_agent_info",
        description: "Get information about the agent's capabilities and state",
        triggers: &[
            "capabilities",
            "what can you do",
            "agent info",
            "tools available",
            "能做什么",
            "功能",
        ],
        pinned: false,
        intents: &[IntentType::Introspect],
        scope: Scope::Local,
        schema_tokens: 30,
    },
    ToolMeta {
        name: "reflect",
        description: "Investigate agent behavior, tool selection, and decision quality",
        triggers: &[
            "reflect",
            "why",
            "investigate",
            "debug agent",
            "what went wrong",
            "反思",
            "为什么",
            "调查",
        ],
        pinned: false,
        intents: &[IntentType::Introspect],
        scope: Scope::Local,
        schema_tokens: 40,
    },
];
