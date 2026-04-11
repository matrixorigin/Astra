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
        schema_tokens: 30,
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
        schema_tokens: 35,
    },
    ToolMeta {
        name: "write_file",
        description: "Create or overwrite a file",
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
        pinned: true,
        intents: &[IntentType::CodeEdit],
        scope: Scope::Local,
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
            "ls",
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
        schema_tokens: 25,
    },
    ToolMeta {
        name: "grep",
        description: "Search file contents with regex patterns",
        triggers: &[
            "search",
            "find",
            "grep",
            "pattern",
            "match",
            "look for",
            "regex",
            "contain",
            "find in files",
            "text search",
            "search code",
            "搜索",
            "查找",
            "查询代码",
            "搜索代码",
            "包含",
            "匹配",
            "代码搜索",
            "全文搜索",
            "搜索文本",
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
        pinned: true,
        intents: &[IntentType::CodeRead],
        scope: Scope::Local,
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
        pinned: false,
        intents: &[IntentType::CodeRead, IntentType::CodeEdit],
        scope: Scope::Local,
        schema_tokens: 90,
    },
    ToolMeta {
        name: "git_status",
        description: "Show git working tree status",
        triggers: &[
            "git status",
            "changes",
            "modified",
            "staged",
            "uncommitted",
            "working tree",
            "untracked",
            "dirty",
            "what changed",
            "pending changes",
            "local changes",
            "unstaged",
            "git state",
            "状态",
            "改了什么",
            "未提交",
            "暂存",
            "改了吗",
            "本地修改",
            "本地改动",
            "有改动吗",
            "什么修改",
        ],
        pinned: false,
        intents: &[IntentType::Git],
        scope: Scope::LocalGit,
        schema_tokens: 15,
    },
    ToolMeta {
        name: "git_diff",
        description: "Show git diffs for files or commits",
        triggers: &[
            "diff",
            "difference",
            "compare",
            "changed",
            "what changed",
            "changes between",
            "review changes",
            "changes in",
            "file changes",
            "差异",
            "对比",
            "变更",
            "改动",
            "看改动",
            "改动了什么",
            "修改了什么",
        ],
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
            "commit history",
            "changelog",
            "recent changes",
            "commit list",
            "review commits",
            "local commits",
            "branch commits",
            "who committed",
            "提交历史",
            "日志",
            "提交记录",
            "最近提交",
            "最近改动",
            "提交列表",
            "谁提交的",
        ],
        pinned: false,
        intents: &[IntentType::Git],
        scope: Scope::LocalGit,
        schema_tokens: 30,
    },
    ToolMeta {
        name: "git_show",
        description: "Show a specific commit's diff, message, author, and changes",
        triggers: &[
            "show commit",
            "commit diff",
            "commit detail",
            "review commit",
            "what changed in",
            "show sha",
            "git show",
            "commit content",
            "inspect commit",
            "查看提交",
            "提交详情",
            "这个提交改了什么",
            "review",
            "评审",
            "提交修改内容",
        ],
        pinned: false,
        intents: &[IntentType::Git],
        scope: Scope::LocalGit,
        schema_tokens: 40,
    },
    ToolMeta {
        name: "git_blame",
        description: "Show who last modified each line of a file with commit info",
        triggers: &[
            "blame",
            "who changed",
            "who wrote",
            "who modified",
            "line author",
            "code ownership",
            "annotate",
            "last modified",
            "who added",
            "who touched",
            "line history",
            "when was this written",
            "谁改的",
            "谁写的",
            "代码归属",
            "行作者",
            "最后修改",
            "谁加的",
            "这行谁写的",
            "代码责任人",
        ],
        pinned: false,
        intents: &[IntentType::Git],
        scope: Scope::LocalGit,
        schema_tokens: 35,
    },
    ToolMeta {
        name: "git_file_history",
        description: "Show change history for a specific file with rename tracking",
        triggers: &[
            "file history",
            "file change history",
            "file log",
            "who changed this file",
            "when was this changed",
            "file evolution",
            "change log",
            "history of",
            "git log file",
            "file commits",
            "how did this evolve",
            "track changes",
            "文件历史",
            "文件变更历史",
            "谁改了这个文件",
            "文件改动记录",
            "这个文件的历史",
            "文件提交记录",
            "文件演变",
        ],
        pinned: false,
        intents: &[IntentType::Git],
        scope: Scope::LocalGit,
        schema_tokens: 30,
    },
    ToolMeta {
        name: "git_contributors",
        description: "Analyze repository contributors, hot files, and activity patterns",
        triggers: &[
            "contributors",
            "who contributed",
            "top authors",
            "hot files",
            "churn",
            "activity",
            "team",
            "codebase analytics",
            "who worked on",
            "author",
            "贡献者",
            "谁贡献的",
            "热点文件",
            "活跃度",
            "团队",
            "代码分析",
            "谁做的",
            "开发者",
        ],
        pinned: false,
        intents: &[IntentType::Git],
        scope: Scope::LocalGit,
        schema_tokens: 30,
    },
    ToolMeta {
        name: "git_log_search",
        description: "Semantic search on commit messages using TF-IDF with CJK support",
        triggers: &[
            "search commits",
            "find commit",
            "when was",
            "commit search",
            "git search",
            "what commit",
            "who committed this",
            "commit about",
            "搜索提交",
            "找提交",
            "什么时候",
            "提交搜索",
            "哪个提交",
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
            "pull request",
            "PRs",
            "list prs",
            "open prs",
            "merged",
            "merge request",
            "github pr",
            "recent prs",
            "show prs",
            "all prs",
            "pending prs",
            "最新的pr",
            "拉取请求",
            "合并请求",
            "pr列表",
            "所有pr",
            "有哪些pr",
            "待合并",
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
            "pr review",
            "merge status",
            "conflicts",
            "pr content",
            "pr changes",
            "what's in pr",
            "pr详情",
            "看看pr",
            "PR状态",
            "合并状态",
            "审查PR",
            "这个pr改了什么",
            "pr内容",
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
            "ci status",
            "pipeline",
            "build status",
            "checks",
            "actions",
            "workflow",
            "ci/cd",
            "github ci",
            "workflow status",
            "build running",
            "github actions",
            "test status",
            "build failed",
            "build passed",
            "check runs",
            "CI状态",
            "构建状态",
            "流水线",
            "工作流状态",
            "构建中吗",
            "测试状态",
            "构建成功了吗",
            "构建失败",
            "CI通过了吗",
        ],
        pinned: false,
        intents: &[IntentType::GitHub],
        scope: Scope::External,
        schema_tokens: 30,
    },
    ToolMeta {
        name: "github_repo_stats",
        description: "Get GitHub repository stars, forks, issues, and metadata",
        triggers: &[
            "stars",
            "star",
            "forks",
            "watchers",
            "repo stats",
            "repository stats",
            "project overview",
            "overview",
            "repo info",
            "about repo",
            "repo description",
            "language breakdown",
            "license",
            "多少star",
            "仓库数据",
            "项目概览",
            "项目数据",
            "仓库信息",
            "仓库描述",
            "用什么语言",
        ],
        pinned: false,
        intents: &[IntentType::GitHub],
        scope: Scope::External,
        schema_tokens: 35,
    },
    ToolMeta {
        name: "github_list_issues",
        description: "List issues for a GitHub repository",
        triggers: &[
            "issues",
            "bugs",
            "list issues",
            "open issues",
            "bug list",
            "issue list",
            "all issues",
            "issues in repo",
            "recent issues",
            "pending issues",
            "问题列表",
            "最新的issue",
            "问题",
            "缺陷",
            "项目问题",
            "所有issue",
            "打开的issue",
            "有哪些issue",
            "待处理问题",
        ],
        pinned: false,
        intents: &[IntentType::GitHub],
        scope: Scope::External,
        schema_tokens: 40,
    },
    ToolMeta {
        name: "github_get_issue",
        description: "Get details of a specific issue",
        triggers: &[
            "issue details",
            "issue #",
            "bug details",
            "specific issue",
            "issue status",
            "show issue",
            "bug report",
            "feature request",
            "read issue",
            "issue content",
            "what's in issue",
            "issue详情",
            "问题详情",
            "查看issue",
            "缺陷详情",
            "issue编号",
            "这个issue",
            "issue内容",
        ],
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
            "open issue",
            "submit issue",
            "write issue",
            "raise issue",
            "post issue",
            "创建issue",
            "新建issue",
            "提issue",
            "提交问题",
            "创建问题",
            "报告bug",
            "新建bug",
        ],
        pinned: false,
        intents: &[IntentType::GitHub],
        scope: Scope::External,
        schema_tokens: 40,
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
        pinned: false,
        intents: &[IntentType::CodeRead],
        scope: Scope::External,
        schema_tokens: 25,
    },
    // memory_store stays pinned: preference expressions ("苹果比较好吃") have
    // zero keyword overlap with memory triggers, so tfidf can't select it.
    // The system prompt tells the LLM to call memory_store for preferences,
    // so the tool must always be available. Cost: ~35 tokens/turn.
    ToolMeta {
        name: "memory_store",
        description: "Store information to persistent memory",
        triggers: &[
            "remember",
            "store",
            "save to memory",
            "memorize",
            "note",
            "keep in mind",
            "persist",
            "archive",
            "关注",
            "follow",
            "watch",
            "track",
            "subscribe",
            "bookmark",
            "记住",
            "保存记忆",
            "存储",
            "记下",
            "记录",
            "保存",
            // Disambiguate from write_file "保存"
            "保存到记忆",
            "存储到记忆",
            "存一下",
            "帮我记住",
        ],
        pinned: true,
        intents: &[IntentType::Memory],
        scope: Scope::CrossSession,
        schema_tokens: 35,
    },
    // memory_search is dynamic: only needed when user explicitly asks about
    // stored memories. Rich triggers ensure tfidf selects it reliably.
    ToolMeta {
        name: "memory_search",
        description: "Search persistent memories for relevant information",
        triggers: &[
            "recall",
            "search memory",
            "what did I",
            "memories",
            "my memory",
            "do I have",
            "what do I",
            "stored",
            "what did I say",
            "preference",
            "preferences",
            "记忆",
            "回忆",
            "搜索记忆",
            "记住了什么",
            "记住的",
            "偏好",
            "偏好是什么",
            "哪些记忆",
            "之前说过",
            "之前记住",
            "搜一下记忆",
            "我的偏好",
            "我记住了",
        ],
        pinned: false,
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
            "clear memory",
            "erase",
            "cleanup",
            "cleanup memory",
            "wipe memory",
            "drop memory",
            "删除记忆",
            "忘记",
            "清除记忆",
            "清理",
            "清理记忆",
            "删掉记忆",
            "不要记住",
        ],
        pinned: false,
        intents: &[IntentType::Memory],
        scope: Scope::CrossSession,
        schema_tokens: 25,
    },
    ToolMeta {
        name: "memory_correct",
        description: "Correct or update an existing memory entry",
        triggers: &[
            "correct memory",
            "update memory",
            "fix memory",
            "amend memory",
            "revise memory",
            "change memory",
            "edit memory",
            "modify memory",
            "that's wrong",
            "not correct",
            "修正记忆",
            "更新记忆",
            "纠正记忆",
            "改记忆",
            "修复记忆",
            "记错了",
            "不对要改",
        ],
        pinned: false,
        intents: &[IntentType::Memory],
        scope: Scope::CrossSession,
        schema_tokens: 30,
    },
    ToolMeta {
        name: "memory_profile",
        description: "Retrieve user profile and preferences from memory",
        triggers: &[
            "my preferences",
            "my profile",
            "user profile",
            "what do you know about me",
            "my settings",
            "my info",
            "about me",
            "user context",
            "my history",
            "用户偏好",
            "用户资料",
            "使用习惯",
            "用户配置",
            "个人信息",
            "你了解我什么",
            "关于我",
        ],
        pinned: false,
        intents: &[IntentType::Memory],
        scope: Scope::CrossSession,
        schema_tokens: 20,
    },
    ToolMeta {
        name: "adjust_config",
        description: "Adjust runtime config values for this session",
        triggers: &["adjust config", "set threshold", "调参数", "配置调整"],
        pinned: false,
        intents: &[IntentType::Introspect],
        scope: Scope::Local,
        schema_tokens: 45,
    },
    ToolMeta {
        name: "prioritize_tool",
        description: "Pin a tool as preferred in this session",
        triggers: &["prioritize tool", "pin tool", "prefer tool", "工具优先"],
        pinned: false,
        intents: &[IntentType::Introspect],
        scope: Scope::Local,
        schema_tokens: 18,
    },
    ToolMeta {
        name: "deprioritize_tool",
        description: "Mark a tool as deprioritized in this session",
        triggers: &["deprioritize tool", "avoid tool", "demote tool", "工具降级"],
        pinned: false,
        intents: &[IntentType::Introspect],
        scope: Scope::Local,
        schema_tokens: 18,
    },
    ToolMeta {
        name: "set_goal",
        description: "Set the current session goal",
        triggers: &["set goal", "session goal", "设置目标", "目标"],
        pinned: false,
        intents: &[IntentType::Introspect, IntentType::Memory],
        scope: Scope::Local,
        schema_tokens: 20,
    },
    ToolMeta {
        name: "compress_context",
        description: "Record a manual context compression request",
        triggers: &[
            "compress context",
            "context too long",
            "shrink context",
            "压缩上下文",
        ],
        pinned: false,
        intents: &[IntentType::Introspect],
        scope: Scope::Local,
        schema_tokens: 18,
    },
    ToolMeta {
        name: "get_agent_info",
        description: "Get information about the agent's capabilities and state",
        triggers: &[
            "capabilities",
            "what can you do",
            "agent info",
            "tools available",
            "help",
            "how to use",
            "features",
            "functions",
            "what tools",
            "list tools",
            "能做什么",
            "功能",
            "帮助",
            "代理能力",
            "代理功能",
            "怎么用",
            "有哪些工具",
            "工具列表",
            "功能列表",
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
            "self-check",
            "diagnose",
            "trace",
            "analyze behavior",
            "what happened",
            "反思",
            "为什么",
            "调查",
            "哪里出错",
            "自检",
            "自我诊断", // Note: 诊断 alone is reserved for LSP code diagnostics
            "排查",
            "怎么回事",
            "问题在哪",
        ],
        pinned: false,
        intents: &[IntentType::Introspect],
        scope: Scope::Local,
        schema_tokens: 40,
    },
    ToolMeta {
        name: "context_analysis",
        description: "Deep analysis of context window composition, token allocation, and budget pressure across turns",
        triggers: &[
            "context analysis",
            "context breakdown",
            "token breakdown",
            "context composition",
            "token allocation",
            "budget pressure",
            "context evolution",
            "context trend",
            "上下文分析",
            "上下文组成",
            "上下文占比",
            "token分布",
            "token占比",
            "上下文变化",
            "预算压力",
        ],
        pinned: false,
        intents: &[IntentType::Introspect],
        scope: Scope::Local,
        schema_tokens: 45,
    },
    // ── MatrixOne tools ─────────────────────────────────────────────
    ToolMeta {
        name: "mo_query",
        description: "Execute SQL query against MatrixOne database for data exploration and analytics",
        triggers: &[
            "sql query",
            "database query",
            "database",
            "matrixone",
            "select from",
            "table",
            "schema",
            "data analytics",
            "aggregate",
            "count rows",
            "statistics",
            "report",
            "数据库",
            "查询数据",
            "表结构",
            "SQL查询",
            "数据分析",
            "统计",
            "聚合",
            "汇总",
        ],
        pinned: false,
        intents: &[IntentType::Database],
        scope: Scope::External,
        schema_tokens: 30,
    },
    ToolMeta {
        name: "mo_snapshot",
        description: "Manage MatrixOne data snapshots for point-in-time recovery and experiments",
        triggers: &[
            "snapshot",
            "checkpoint",
            "data checkpoint",
            "save state",
            "restore",
            "rollback",
            "backup",
            "time travel",
            "快照",
            "数据快照",
            "备份",
            "回滚",
            "恢复",
        ],
        pinned: false,
        intents: &[IntentType::Database],
        scope: Scope::External,
        schema_tokens: 30,
    },
    ToolMeta {
        name: "mo_branch",
        description: "Coordinate git branches with MatrixOne data branches for experiment isolation",
        triggers: &[
            "data branch",
            "database branch",
            "experiment",
            "isolate data",
            "sync branch",
            "branch data",
            "experiment branch",
            "数据分支",
            "实验分支",
            "分支数据",
            "隔离",
        ],
        pinned: false,
        intents: &[IntentType::Database, IntentType::Git],
        scope: Scope::External,
        schema_tokens: 30,
    },
    // ── Tool Chain (multi-step composition) ──────────────────────────
    ToolMeta {
        name: "run_chain",
        description: "Execute a multi-step tool chain with variable passing between steps",
        triggers: &[
            "chain",
            "tool chain",
            "multi-step",
            "run workflow",
            "sequence",
            "compose tools",
            "step by step",
            "orchestrate",
            "batch tools",
            "pipeline",
            "then",
            "after that",
            "followed by",
            "in order",
            "sequential",
            "链式",
            "多步",
            "编排工具",
            "编排",
            "组合工具",
            "顺序执行",
            "流水线",
            "然后",
            "接着",
            "依次",
        ],
        pinned: false,
        intents: &[IntentType::CodeEdit, IntentType::CodeRead],
        scope: Scope::Local,
        schema_tokens: 80,
    },
    // ── Delegation (multi-agent coordination) ────────────────────────
    ToolMeta {
        name: "delegate",
        description: "Delegate tasks to specialized sub-agents for parallel or coordinated execution",
        triggers: &[
            "delegate",
            "multi-agent",
            "multiple agents",
            "agents help",
            "have agents",
            "parallel agents",
            "sub-agent",
            "subagent",
            "fan out",
            "fan-out",
            "distribute work",
            "team up",
            "coordinate agents",
            "split task",
            // Natural language patterns
            "agents do",
            "agents analyze",
            "agents check",
            "agents review",
            "use agents",
            "send to agents",
            "spawn agents",
            "run in parallel",
            "parallelize",
            // Chinese patterns
            "委托",
            "多agent",
            "多智能体",
            "并行分析",
            "分发任务",
            "协作",
            "让agent帮我",
            "几个agent",
            "多个agent",
            "分工",
            "协同工作",
            "派agent",
            "启动agent",
            "用agent",
            "agent帮忙",
            "agent去做",
            "agent分析",
            "agent检查",
            "并行执行",
            "并发处理",
        ],
        pinned: false, // Actually injected dynamically when delegation_engine is present
        intents: &[IntentType::CodeEdit, IntentType::CodeRead],
        scope: Scope::Local,
        schema_tokens: 120,
    },
];

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
        // Sanity check — if tools are added/removed, update this
        assert!(
            TOOL_CATALOG.len() >= 30,
            "expected at least 30 tools, got {}",
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
    fn catalog_memory_store_is_pinned() {
        let ms = TOOL_CATALOG
            .iter()
            .find(|t| t.name == "memory_store")
            .unwrap();
        assert!(ms.pinned, "memory_store should be pinned");
    }
}
