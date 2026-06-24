//! Unified slash command registry — single source of truth for command metadata.
//!
//! This module consolidates:
//! - Command names and descriptions
//! - Group categorization (with icons)
//! - Aliases
//! - Subcommand completions
//! - Argument hints
//!
//! All slash command metadata should be defined here. Other modules (TUI slash
//! menus, main.rs) should query this registry rather than maintaining their
//! own static arrays.

use crate::cli::command_usage;

/// Command groups for organizing the help palette.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CommandGroup {
    Core,
    Workspace,
    Observability,
    SessionPlan,
    MemoryTasks,
    Skills,
    Mcp,
    TeamAccount,
    System,
}

impl CommandGroup {
    /// All groups in display order.
    pub const ALL: &'static [CommandGroup] = &[
        CommandGroup::Core,
        CommandGroup::Workspace,
        CommandGroup::Observability,
        CommandGroup::SessionPlan,
        CommandGroup::MemoryTasks,
        CommandGroup::Skills,
        CommandGroup::Mcp,
        CommandGroup::TeamAccount,
        CommandGroup::System,
    ];

    /// Icon for this group (emoji).
    pub const fn icon(&self) -> &'static str {
        match self {
            CommandGroup::Core => "⚡",
            CommandGroup::Workspace => "📂",
            CommandGroup::Observability => "🔭",
            CommandGroup::SessionPlan => "📋",
            CommandGroup::MemoryTasks => "🧠",
            CommandGroup::Skills => "📦",
            CommandGroup::Mcp => "🔌",
            CommandGroup::TeamAccount => "👥",
            CommandGroup::System => "🔧",
        }
    }

    /// Display title for this group.
    pub const fn title(&self) -> &'static str {
        match self {
            CommandGroup::Core => "Core",
            CommandGroup::Workspace => "Workspace",
            CommandGroup::Observability => "Observability",
            CommandGroup::SessionPlan => "Session & plan",
            CommandGroup::MemoryTasks => "Memory & tasks",
            CommandGroup::Skills => "Skills",
            CommandGroup::Mcp => "MCP",
            CommandGroup::TeamAccount => "Team & account",
            CommandGroup::System => "System",
        }
    }
}

/// How a slash command is handled inside the TUI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TuiHandler {
    /// Opens a native TUI panel (e.g. /context → ContextPanel).
    Panel,
    /// Opens a TUI selector or picker (e.g. /model → model picker).
    Selector,
    /// Sent as chat input to the LLM (original REPL behavior).
    ChatForward,
    /// Handled inline in the dispatch function (e.g. /exit, /help).
    Inline,
    /// Tears down the TUI, runs the legacy slash handler, then restores the TUI.
    Fallback,
}

/// Metadata for a single slash command.
#[derive(Debug, Clone, Copy)]
pub struct CommandMeta {
    /// The command name, including the leading slash (e.g., "/help").
    pub name: &'static str,
    /// Short description shown in help.
    pub description: &'static str,
    /// Which group this command belongs to.
    pub group: CommandGroup,
    /// Whether this is an alias for another command.
    pub is_alias: bool,
    /// Subcommand completions (token, description) for Tab completion.
    pub subcommands: &'static [(&'static str, &'static str)],
    /// Argument hint shown inline (e.g., "<name>").
    pub arg_hint: Option<&'static str>,
    /// How this command is handled inside the TUI.
    pub tui_handler: TuiHandler,
    /// Usage examples for the help display (without leading `/`).
    pub usage_examples: &'static [&'static str],
}

impl CommandMeta {
    /// Create a new command with defaults.
    pub const fn new(name: &'static str, description: &'static str, group: CommandGroup) -> Self {
        Self {
            name,
            description,
            group,
            is_alias: false,
            subcommands: &[],
            arg_hint: None,
            tui_handler: TuiHandler::ChatForward,
            usage_examples: &[],
        }
    }

    /// Mark as an alias.
    pub const fn alias(mut self) -> Self {
        self.is_alias = true;
        self
    }

    /// Add subcommands for Tab completion.
    pub const fn with_subcommands(
        mut self,
        subcommands: &'static [(&'static str, &'static str)],
    ) -> Self {
        self.subcommands = subcommands;
        self
    }

    /// Add argument hint.
    pub const fn with_arg_hint(mut self, hint: &'static str) -> Self {
        self.arg_hint = Some(hint);
        self
    }

    /// Set how this command is handled inside the TUI.
    pub const fn with_tui_handler(mut self, handler: TuiHandler) -> Self {
        self.tui_handler = handler;
        self
    }

    /// Add usage examples for help display.
    pub const fn with_usage_examples(mut self, examples: &'static [&'static str]) -> Self {
        self.usage_examples = examples;
        self
    }
}

// ── Subcommand completion arrays ────────────────────────────────────────────

const MODEL_SUBCOMMANDS: &[(&str, &str)] = &[
    ("info", "Show details for the current model"),
    ("list", "Open the picker to choose a model"),
    ("clear", "Clear the active model selection"),
];

const STATS_SUBCOMMANDS: &[(&str, &str)] = &[
    ("cost", "Per-session API cost estimate"),
    ("health", "Tool health dashboard"),
    ("history", "Aggregate stats across recent sessions"),
    ("learn", "Learning insights: patterns, drift, exploration"),
    ("tools", "Tool performance: calls, timing, success rate"),
];

const HEALTH_SUBCOMMANDS: &[(&str, &str)] = &[("detail", "Per-tool breakdown")];

const SYNC_SUBCOMMANDS: &[(&str, &str)] = &[("log", "Server-owned sync log hint")];

const REVIEW_SUBCOMMANDS: &[(&str, &str)] = &[
    ("latest", "Review HEAD (default)"),
    ("working", "Review working tree vs HEAD"),
];

const SKILL_SUBCOMMANDS: &[(&str, &str)] = &[
    ("browse", "Browse marketplace"),
    ("create", "Generate skill from session"),
    ("dev", "Skill dev mode"),
    ("feedback", "Record user feedback (+/-)"),
    ("health", "Skill catalog health"),
    ("info", "Skill details"),
    ("installed", "List installed marketplace skills"),
    ("install", "Install from marketplace"),
    ("list", "List skills"),
    ("new", "Create skill"),
    ("publish", "Publish to marketplace"),
    ("rollback", "Rollback installed skill version"),
    ("search", "Keyword search catalog"),
    ("stats", "Learning summary"),
    ("surfacing", "Agent catalog surfacing (dynamic/min/cap)"),
    ("system", "System skill helpers"),
    ("test", "Run skill test"),
    ("trending", "Show trending marketplace skills"),
    ("uninstall", "Remove local skill"),
    ("upgrade", "Upgrade installed skill version"),
];

const MCP_SUBCOMMANDS: &[(&str, &str)] = &[
    ("help", "Show MCP commands with examples"),
    ("list", "Overview: servers, tools, prompts, resources"),
    ("servers", "Server details and tool counts"),
    ("status", "Alias for /mcp list"),
    ("tools", "All callable tools (or: tools <server>)"),
    ("inspect", "Tool schema: /mcp inspect <server>:<tool>"),
    ("prompts", "List prompt templates from MCP servers"),
    ("resources", "List readable MCP resources"),
    ("read", "Read a resource: /mcp read <server>:<uri>"),
    ("ping", "Ping: /mcp ping [server]"),
    ("history", "Recent MCP tool-call history"),
    ("add", "Add server: /mcp add <name> <command> [args…]"),
    ("remove", "Remove server: /mcp remove <name>"),
    (
        "prompt",
        "Invoke prompt: /mcp prompt <server>:<name> [args]",
    ),
    (
        "complete",
        "Arg completions: /mcp complete <server>:prompt:<name> <arg> [value]",
    ),
    (
        "log-level",
        "Set log level: /mcp log-level <server> <level>",
    ),
    (
        "subscribe",
        "Subscribe to resource: /mcp subscribe <server>:<uri>",
    ),
    (
        "unsubscribe",
        "Unsubscribe: /mcp unsubscribe <server>:<uri>",
    ),
];

const TASK_SUBCOMMANDS: &[(&str, &str)] = &[
    ("list", "List background tasks"),
    ("pending", "List claimable task queue"),
    ("run", "Run a background task prompt"),
    ("result", "Task result (needs id)"),
    ("status", "Task status (needs id/query)"),
];

const MEMORY_SUBCOMMANDS: &[(&str, &str)] = &[
    // ── Browse ──
    ("list", "List memories grouped by type"),
    ("ls", "Alias for list"),
    ("search", "Search memories by content (needs query)"),
    ("show", "Inspect one memory in detail (needs id)"),
    ("inspect", "Alias for show (needs id)"),
    ("stats", "Count memories by type"),
    (
        "dismiss",
        "Lower retrieval score for matching memories (needs query)",
    ),
    ("help", "Show the full /memory help surface"),
    // ── Session ──
    ("session", "Show current session memory"),
    ("edit", "Edit a session memory section (needs section)"),
    // ── Manage ──
    ("forget", "Delete a memory (needs id)"),
    ("snapshot", "Create a memory checkpoint"),
    ("rollback", "Restore to a memory checkpoint (needs name)"),
    ("snapshots", "List all memory checkpoints"),
    // ── Branches ──
    ("branch", "Create an experiment memory branch (needs name)"),
    ("checkout", "Switch to a memory branch (needs name)"),
    ("merge", "Merge a branch back into main (needs name)"),
    ("diff", "Preview branch or snapshot changes (needs name)"),
    ("branches", "List all memory branches"),
    // ── Analysis ──
    ("reflect", "Analyze memory patterns"),
    ("health", "Memory hygiene status"),
];

const PROFILE_SUBCOMMANDS: &[(&str, &str)] = &[
    ("show", "Show the current user profile"),
    ("edit", "Edit a preference"),
    ("scenario", "Show the detected working scenario"),
    ("stats", "Show profile usage stats"),
    ("tools", "Show blocked tool policy"),
    ("experiments", "Show enrolled experiments"),
    ("reset", "Reset profile preferences"),
    ("help", "Show profile help"),
];

// Subcommands surfaced by the `/session ` popup.  Kept tight so
// completion shows only the high-value entry points; rarer
// diagnostic forms (cleanup / drift / errors / trace / verify /
// adaptive / switch) still work via the line-mode fallback but
// aren't advertised — most users reach them through dedicated
// slash commands or the /diag tooling instead.
const SESSION_SUBCOMMANDS: &[(&str, &str)] = &[
    ("analyze", "Counter-only diagnostics for a session"),
    ("export", "Write a markdown transcript to disk"),
    ("fork", "Branch a parallel session from a parent"),
    ("history", "Scroll a session's conversation history"),
    ("list", "Pick a session to resume"),
];

const DIFF_SUBCOMMANDS: &[(&str, &str)] = &[
    ("help", "Diff usage"),
    ("patch", "Unstaged diff alias"),
    ("show", "git show <rev> (needs rev)"),
    ("staged", "Staged vs HEAD"),
    ("stat", "Diff stat vs HEAD"),
    ("unstaged", "Unstaged only"),
];

// TURN_SUBCOMMANDS removed — /turn merged into /timeline

// EXPERIMENT_SUBCOMMANDS removed — /experiment is dead code

const ALLOW_SUBCOMMANDS: &[(&str, &str)] = &[
    ("auto", "Auto-approve all tool use"),
    ("plan", "Read-only investigation mode; mutations are denied"),
    (
        "accept_edits",
        "Auto-approve workspace-local file edits while still prompting for shell and external writes",
    ),
    ("deny", "Deny all tool use"),
    ("prompt", "Prompt before tool use"),
    ("rules", "Show current permission rules"),
    ("trust", "Trust this workspace"),
    ("untrust", "Mark this workspace untrusted"),
    ("trace", "Show recent permission audit events"),
];

const INSTRUCTIONS_SUBCOMMANDS: &[(&str, &str)] = &[
    ("off", "Disable project instructions for this session"),
    ("reload", "Reload from .astra/instructions.md"),
    ("show", "Show loaded project instructions"),
];

const TEAM_SUBCOMMANDS: &[(&str, &str)] = &[
    ("add-member", "Add member to team"),
    ("context", "Set shared context for team"),
    ("create", "Create new team"),
    ("delete", "Delete a team"),
    ("help", "Show team overview and examples"),
    ("history", "Show team execution history"),
    ("info", "Show team information"),
    ("list", "List all teams"),
    ("restore", "Restore team snapshot"),
    ("run", "Run a task with the team"),
    ("snapshot", "Save a team snapshot"),
];

const AGENT_SUBCOMMANDS: &[(&str, &str)] = &[
    ("help", "Show agent help"),
    ("list", "List spawned agents"),
    ("logs", "Show agent logs"),
    ("status", "Show agent status"),
    ("stop", "Stop an agent"),
];

const MESSAGING_SUBCOMMANDS: &[(&str, &str)] = &[
    ("dlq", "Show dead letter queue"),
    ("help", "Show messaging help"),
    ("metrics", "Show metrics snapshot"),
    ("status", "Show mailbox status"),
];

const COMPACT_SUBCOMMANDS: &[(&str, &str)] = &[
    ("no-memoria", "Compact without Memoria"),
    ("quick", "Fast compaction without summary"),
    ("summary-only", "Summarize without trimming"),
];

// TUNING_SUBCOMMANDS removed — evolution subsystem deleted

const CONFIG_SUBCOMMANDS: &[(&str, &str)] = &[
    ("diff", "Show differences from defaults"),
    ("export", "Export configuration to file"),
    ("paths", "Show config file paths"),
    ("show", "Show current configuration"),
    ("sources", "Show where each value came from"),
];

const HELP_SUBCOMMANDS: &[(&str, &str)] = &[("keys", "Keyboard shortcuts")];

// ── The unified command registry ────────────────────────────────────────────

/// All registered slash commands.
pub static COMMANDS: &[CommandMeta] = &[
    // ── Core ──────────────────────────────────────────────────────────────
    CommandMeta::new(
        "/help",
        "Show available commands; /help keys for shortcuts",
        CommandGroup::Core,
    )
    .with_subcommands(HELP_SUBCOMMANDS)
    .with_tui_handler(TuiHandler::Inline),
    CommandMeta::new(
        "/model",
        "Open the model picker, show current model, or switch",
        CommandGroup::Core,
    )
    .with_subcommands(MODEL_SUBCOMMANDS)
    .with_arg_hint("[info | list | clear | <name>]")
    .with_tui_handler(TuiHandler::Selector),
    CommandMeta::new("/clear", "Start a new session", CommandGroup::Core)
        .with_tui_handler(TuiHandler::Fallback),
    CommandMeta::new("/undo", "Undo last turn(s): /undo [N]", CommandGroup::Core)
        .with_arg_hint("[N]")
        .with_tui_handler(TuiHandler::Fallback),
    CommandMeta::new(
        "/redo",
        "Redo undone turn(s): /redo [N]",
        CommandGroup::Core,
    )
    .with_arg_hint("[N]")
    .with_tui_handler(TuiHandler::Fallback),
    CommandMeta::new(
        "/checkpoint",
        "Manual save: /checkpoint [label] — JSON + session md + journal",
        CommandGroup::Core,
    )
    .with_arg_hint("[label]")
    .with_tui_handler(TuiHandler::Fallback),
    CommandMeta::new(
        "/history",
        "Conversation turns; /history grep <q> filters in-memory",
        CommandGroup::Core,
    ),
    CommandMeta::new(
        "/copy",
        "Copy last response to clipboard",
        CommandGroup::Core,
    ),
    CommandMeta::new(
        "/resume",
        "Resume a session: /resume [session_id]",
        CommandGroup::Core,
    )
    .with_arg_hint("[session_id]")
    .with_tui_handler(TuiHandler::Panel),
    CommandMeta::new(
        "/timeline",
        "Browse this session's turn-by-turn journal timeline",
        CommandGroup::Core,
    )
    .with_tui_handler(TuiHandler::Panel),
    CommandMeta::new(
        "/table",
        "Run a SQL query and render the result as a navigable table",
        CommandGroup::Core,
    )
    .with_arg_hint("<sql>")
    .with_tui_handler(TuiHandler::Panel),
    CommandMeta::new(
        "/worktrees",
        "List git worktrees for this repo with per-worktree session counts",
        CommandGroup::Core,
    )
    .with_tui_handler(TuiHandler::Panel),
    CommandMeta::new(
        "/panels",
        "Cheat sheet of all TUI-native panels",
        CommandGroup::Core,
    )
    .with_tui_handler(TuiHandler::Inline),
    CommandMeta::new("/exit", "Exit astra", CommandGroup::Core)
        .with_tui_handler(TuiHandler::Inline),
    CommandMeta::new("/quit", "Exit astra (alias for /exit)", CommandGroup::Core).alias(),
    // ── Workspace ─────────────────────────────────────────────────────────
    CommandMeta::new(
        "/grep",
        "Workspace ripgrep: <pattern> | files <glob> | review <pattern>",
        CommandGroup::Workspace,
    )
    .with_arg_hint("<pattern>")
    .with_tui_handler(TuiHandler::Fallback),
    CommandMeta::new(
        "/diff",
        "Colored git diff (staged, stat, show <rev>, …)",
        CommandGroup::Workspace,
    )
    .with_subcommands(DIFF_SUBCOMMANDS)
    .with_arg_hint("[staged|unstaged|stat|show <rev>]")
    .with_tui_handler(TuiHandler::Fallback),
    CommandMeta::new(
        "/review",
        "LLM review of git changes: /review [latest|<rev>|working]",
        CommandGroup::Workspace,
    )
    .with_subcommands(REVIEW_SUBCOMMANDS)
    .with_arg_hint("[latest|<rev>|working]")
    .with_tui_handler(TuiHandler::Fallback),
    // ── Session & plan ───────────────────────────────────────────────────
    CommandMeta::new(
        "/session",
        "Open the session hub, or run a subcommand",
        CommandGroup::SessionPlan,
    )
    .with_subcommands(SESSION_SUBCOMMANDS)
    .with_arg_hint("[list | history | fork | analyze | export]"),
    // session sub-commands — aliases, no TUI handler needed
    CommandMeta::new(
        "/session history",
        "Session journal-style history",
        CommandGroup::SessionPlan,
    ),
    CommandMeta::new(
        "/session errors",
        "Session errors from journal",
        CommandGroup::SessionPlan,
    ),
    CommandMeta::new(
        "/session export",
        "Export session to timestamped Markdown in cwd",
        CommandGroup::SessionPlan,
    ),
    CommandMeta::new(
        "/session fork",
        "Fork session — new id, copy journal (multi-agent / experiments)",
        CommandGroup::SessionPlan,
    ),
    CommandMeta::new(
        "/session list",
        "All journals + cwd / git / age from workspace",
        CommandGroup::SessionPlan,
    ),
    CommandMeta::new(
        "/session cleanup",
        "Clean stale sessions: --days N, --force, --compress",
        CommandGroup::SessionPlan,
    ),
    CommandMeta::new(
        "/session verify",
        "Verify session integrity",
        CommandGroup::SessionPlan,
    ),
    CommandMeta::new(
        "/plan",
        "Enter plan mode; optionally start with a description",
        CommandGroup::SessionPlan,
    )
    .with_arg_hint("[description]")
    .with_tui_handler(TuiHandler::Inline),
    CommandMeta::new(
        "/report",
        "Last delivery report (/report save = JSON)",
        CommandGroup::SessionPlan,
    )
    .with_tui_handler(TuiHandler::Fallback),
    // ── Memory & tasks ────────────────────────────────────────────────────
    CommandMeta::new(
        "/memory",
        "Memoria: browse/search/stats in-panel; health pane; details/manage text-first",
        CommandGroup::MemoryTasks,
    )
    .with_subcommands(MEMORY_SUBCOMMANDS)
    .with_arg_hint("[list|ls|search <q>|stats|show <id>|session|help]"),
    CommandMeta::new(
        "/task",
        "Background tasks: list, pending, status, run <prompt>, result <id>",
        CommandGroup::MemoryTasks,
    )
    .with_subcommands(TASK_SUBCOMMANDS)
    .with_arg_hint("[list|pending|status <id>|run <prompt>|result <id>]")
    .with_tui_handler(TuiHandler::Fallback),
    // ── Observability ─────────────────────────────────────────────────────
    CommandMeta::new(
        "/explain",
        "Cycle explain: off → on (API) → verbose (+stderr)",
        CommandGroup::Observability,
    )
    .with_tui_handler(TuiHandler::Fallback),
    CommandMeta::new(
        "/verbose",
        "(removed — use /stats)",
        CommandGroup::Observability,
    )
    .alias(),
    CommandMeta::new(
        "/compact",
        "Summarize & trim history (quick | no-memoria, …)",
        CommandGroup::Observability,
    )
    .with_subcommands(COMPACT_SUBCOMMANDS)
    .with_arg_hint("[quick|no-memoria|summary-only]")
    .with_tui_handler(TuiHandler::Fallback),
    CommandMeta::new(
        "/reflect",
        "Reflect on session (modes: skill_failure, performance, …)",
        CommandGroup::Observability,
    )
    .with_tui_handler(TuiHandler::Fallback),
    CommandMeta::new(
        "/turn",
        "(removed — use /timeline, Enter to drill into a turn)",
        CommandGroup::Observability,
    )
    .alias(),
    CommandMeta::new(
        "/debug",
        "Developer-oriented session debugger: messages, tools, injections",
        CommandGroup::Observability,
    )
    .with_tui_handler(TuiHandler::Fallback),
    CommandMeta::new(
        "/cache",
        "Prompt-cache summary and diagnosis for the active session",
        CommandGroup::Observability,
    )
    .with_arg_hint("[diagnosis|diag|detail]")
    .with_tui_handler(TuiHandler::Fallback),
    CommandMeta::new(
        "/inspect",
        "Harness snapshots and exports: budget, tools, context, diff, trace, …",
        CommandGroup::Observability,
    )
    .with_arg_hint("[budget|tools|context|cache|json|diff|history N|trace|forensics|export path]"),
    CommandMeta::new(
        "/stats",
        "Session analytics: overview, history, tools, cost, health, learning",
        CommandGroup::Observability,
    )
    .with_subcommands(STATS_SUBCOMMANDS)
    .with_arg_hint("[cost|health|history|learn|tools]")
    .with_tui_handler(TuiHandler::Selector),
    CommandMeta::new(
        "/lsp",
        "LSP backend status: /lsp [status]",
        CommandGroup::Observability,
    )
    .with_arg_hint("[status]")
    .with_tui_handler(TuiHandler::Fallback),
    CommandMeta::new(
        "/telemetry",
        "Deep observability traces: turns, drift, decisions, profile, context",
        CommandGroup::Observability,
    )
    .with_tui_handler(TuiHandler::Fallback),
    CommandMeta::new(
        "/tuning",
        "(removed — evolution subsystem deleted)",
        CommandGroup::Observability,
    )
    .alias(),
    CommandMeta::new(
        "/config",
        "Runtime config: panel for edit; show|paths|sources|diff|export stay text-first",
        CommandGroup::Observability,
    )
    .with_subcommands(CONFIG_SUBCOMMANDS)
    .with_arg_hint("[edit|show|paths|sources|diff|export [path]]")
    .with_tui_handler(TuiHandler::Panel),
    CommandMeta::new(
        "/sync",
        "Cloud sync status (server-owned)",
        CommandGroup::Observability,
    )
    .with_subcommands(SYNC_SUBCOMMANDS)
    .with_arg_hint("[log|push|pull]")
    .with_tui_handler(TuiHandler::Fallback),
    CommandMeta::new(
        "/context",
        "Open the context panel (TUI) or dump a snapshot to disk",
        CommandGroup::Observability,
    )
    .with_subcommands(&[("dump", "Write a JSON snapshot of the live context to disk")])
    .with_arg_hint("[dump [path]]")
    .with_tui_handler(TuiHandler::Panel),
    CommandMeta::new(
        "/rewind",
        "Rewind conversation to an earlier turn",
        CommandGroup::Observability,
    )
    .with_arg_hint("<turn>")
    .with_tui_handler(TuiHandler::Fallback),
    CommandMeta::new("/version", "Version info", CommandGroup::Observability),
    CommandMeta::new(
        "/info",
        "System info at a glance: version, session, model, permissions, skills",
        CommandGroup::Observability,
    )
    .with_tui_handler(TuiHandler::Panel),
    CommandMeta::new("/whoami", "Alias for /info", CommandGroup::Observability)
        .alias()
        .with_tui_handler(TuiHandler::Panel),
    CommandMeta::new(
        "/health",
        "Alias for /stats health",
        CommandGroup::Observability,
    )
    .alias()
    .with_subcommands(HEALTH_SUBCOMMANDS)
    .with_arg_hint("[detail]")
    .with_tui_handler(TuiHandler::Fallback),
    CommandMeta::new(
        "/experiment",
        "(removed — use /profile experiments)",
        CommandGroup::Observability,
    )
    .alias(),
    // ── Skills ────────────────────────────────────────────────────────────
    CommandMeta::new(
        "/skill",
        "Skills: browse|list|info|install|publish|search|new|test|dev|system|…",
        CommandGroup::Skills,
    )
    .with_subcommands(SKILL_SUBCOMMANDS)
    .with_arg_hint("[browse|list|info|install|publish|search|new|test|dev|feedback|…]")
    .with_tui_handler(TuiHandler::Selector),
    // ── MCP ───────────────────────────────────────────────────────────────
    CommandMeta::new(
        "/mcp",
        "MCP: list|tools|inspect|prompts|resources|read|ping|add|remove|…",
        CommandGroup::Mcp,
    )
    .with_subcommands(MCP_SUBCOMMANDS)
    .with_arg_hint("[subcommand]  e.g. tools, inspect <server>:<tool>")
    .with_usage_examples(&[
        "mcp list",
        "mcp tools",
        "mcp tools <server>",
        "mcp inspect <server>:<tool>",
        "mcp read <server>:<uri>",
        "mcp ping [server]",
        "mcp add <name> <command> [args…]",
        "mcp remove <name>",
    ])
    .with_tui_handler(TuiHandler::Fallback),
    // ── Team & account ───────────────────────────────────────────────────
    CommandMeta::new(
        "/team",
        "Teams: list|info|create|add-member|context|run|history|snapshot|restore|delete|help",
        CommandGroup::TeamAccount,
    )
    .with_subcommands(TEAM_SUBCOMMANDS)
    .with_arg_hint("[list|info|create|add-member|context|run|…]")
    .with_tui_handler(TuiHandler::Fallback),
    CommandMeta::new(
        "/agent",
        "Spawned agents: list, status, stop, logs",
        CommandGroup::TeamAccount,
    )
    .with_subcommands(AGENT_SUBCOMMANDS)
    .with_arg_hint("[list|status|stop|logs|help]")
    .with_tui_handler(TuiHandler::Fallback),
    CommandMeta::new(
        "/messaging",
        "Inter-agent messaging: metrics, dlq, status",
        CommandGroup::TeamAccount,
    )
    .with_subcommands(MESSAGING_SUBCOMMANDS)
    .with_arg_hint("[metrics|dlq|status|help]")
    .with_tui_handler(TuiHandler::Fallback),
    CommandMeta::new(
        "/login",
        "Authenticate with the API",
        CommandGroup::TeamAccount,
    )
    .with_tui_handler(TuiHandler::Panel),
    CommandMeta::new(
        "/register",
        "Register a new account",
        CommandGroup::TeamAccount,
    )
    .with_tui_handler(TuiHandler::Panel),
    CommandMeta::new("/logout", "Logout from the API", CommandGroup::TeamAccount)
        .with_tui_handler(TuiHandler::Fallback),
    CommandMeta::new(
        "/profile",
        "Profile preferences: show, edit, scenario, stats, tools, experiments, reset",
        CommandGroup::TeamAccount,
    )
    .with_subcommands(PROFILE_SUBCOMMANDS)
    .with_arg_hint("[show|edit <key> <value>|scenario|stats|tools|experiments|reset]")
    .with_tui_handler(TuiHandler::Fallback),
    CommandMeta::new(
        "/memory-setup",
        "Guided Memoria configuration",
        CommandGroup::TeamAccount,
    )
    .with_tui_handler(TuiHandler::Fallback),
    // ── System ────────────────────────────────────────────────────────────
    CommandMeta::new(
        "/allow",
        "Permission mode: /allow [auto|plan|accept_edits|prompt|deny|rules|trust|untrust|trace]",
        CommandGroup::System,
    )
    .with_subcommands(ALLOW_SUBCOMMANDS)
    .with_arg_hint("[auto|accept_edits|plan|prompt|deny|rules|trust|untrust|trace]")
    .with_tui_handler(TuiHandler::Inline),
    CommandMeta::new(
        "/instructions",
        "Project instructions: /instructions [show|reload|off]",
        CommandGroup::System,
    )
    .with_subcommands(INSTRUCTIONS_SUBCOMMANDS)
    .with_arg_hint("[show|reload|off]"),
    CommandMeta::new(
        "/diagnostics",
        "Binary, API, auth, environment checks",
        CommandGroup::System,
    )
    .with_arg_hint("— run all checks")
    .with_tui_handler(TuiHandler::Fallback),
    // Note: /lsp is in Observability group, not duplicated here
    CommandMeta::new(
        "/bug",
        "Generate bug report: /bug [copy|save]",
        CommandGroup::System,
    )
    .with_arg_hint("[copy|save]")
    .with_tui_handler(TuiHandler::Fallback),
];

// ── Query functions ─────────────────────────────────────────────────────────

/// Resolve a command input to an exact command name.
/// Returns Ok(name) if exact match or unique prefix match.
/// Returns Err(candidates) if ambiguous or no match.
pub fn resolve_command(input: &str) -> Result<&'static str, Vec<&'static str>> {
    // Exact match
    if let Some(meta) = COMMANDS.iter().find(|m| m.name == input) {
        return Ok(meta.name);
    }
    // Prefix match
    let mut matches: Vec<&'static str> = COMMANDS
        .iter()
        .map(|m| m.name)
        .filter(|name| name.starts_with(input))
        .collect();
    matches.sort_unstable();
    matches.dedup();
    if matches.len() == 1 {
        Ok(matches[0])
    } else {
        Err(matches)
    }
}

/// Resolve a command input to its full metadata, including TUI handler info.
/// Returns `None` when the command can't be resolved.
pub fn resolve_command_meta(input: &str) -> Option<&'static CommandMeta> {
    let name = resolve_command(input).ok()?;
    COMMANDS.iter().find(|m| m.name == name)
}

/// Suggest commands similar to the input (for typo correction / fuzzy matching).
pub fn suggest_commands(input: &str, limit: usize) -> Vec<&'static str> {
    let mut scored: Vec<(usize, bool, usize, &'static str)> = COMMANDS
        .iter()
        .map(|m| {
            (
                suggestion_score(m.name, input).saturating_add(command_usage::usage_boost(m.name)),
                m.is_alias,
                m.name.len(),
                m.name,
            )
        })
        .filter(|(score, _, _, _)| *score > 0)
        .collect();
    scored.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| a.1.cmp(&b.1))
            .then_with(|| a.2.cmp(&b.2))
            .then_with(|| a.3.cmp(b.3))
    });
    scored
        .into_iter()
        .take(limit)
        .map(|(_, _, _, cmd)| cmd)
        .collect()
}

fn suggestion_score(command: &str, query: &str) -> usize {
    let cmd_lower = command.trim_start_matches('/').to_ascii_lowercase();
    let query_lower = query.trim_start_matches('/').to_ascii_lowercase();
    if query_lower.is_empty() {
        return 0;
    }
    if cmd_lower == query_lower {
        return 20_000;
    }
    if cmd_lower.starts_with(&query_lower) {
        return 10_000 + (100_usize.saturating_sub(cmd_lower.len().min(100)));
    }
    if cmd_lower.contains(&query_lower) {
        return 5_000 + (100_usize.saturating_sub(cmd_lower.len().min(100)));
    }

    let mut query_chars = query_lower.chars().peekable();
    let mut consecutive = 0usize;
    let mut score = 0usize;
    for ch in cmd_lower.chars() {
        if query_chars.peek() == Some(&ch) {
            query_chars.next();
            consecutive += 1;
            score += consecutive;
        } else {
            consecutive = 0;
        }
    }

    if query_chars.peek().is_none() {
        1_000 + score + (100_usize.saturating_sub(cmd_lower.len().min(100)))
    } else {
        0
    }
}

/// Get completion candidates for a command prefix.
/// Returns (name, description) tuples sorted appropriately.
pub fn completion_candidates(prefix: &str) -> Vec<(&'static str, &'static str)> {
    let mut rows: Vec<(&'static str, &'static str)> = COMMANDS
        .iter()
        .filter(|m| m.name.starts_with(prefix))
        .map(|m| (m.name, m.description))
        .collect();
    // Sort: non-aliases first, then aliases, then by name
    rows.sort_by(|(a_name, _), (b_name, _)| {
        let a_usage = command_usage::usage_count(a_name);
        let b_usage = command_usage::usage_count(b_name);
        let a_alias = COMMANDS
            .iter()
            .find(|m| m.name == *a_name)
            .is_some_and(|m| m.is_alias);
        let b_alias = COMMANDS
            .iter()
            .find(|m| m.name == *b_name)
            .is_some_and(|m| m.is_alias);
        b_usage
            .cmp(&a_usage)
            .then_with(|| a_alias.cmp(&b_alias))
            .then_with(|| a_name.cmp(b_name))
    });
    rows
}

/// Get subcommand completions for a parent command.
pub fn subcommand_completions(parent: &str) -> Option<&'static [(&'static str, &'static str)]> {
    COMMANDS
        .iter()
        .find(|m| m.name == parent && !m.subcommands.is_empty())
        .map(|m| m.subcommands)
}

/// Get commands belonging to a specific group.
pub fn commands_by_group(group: CommandGroup) -> impl Iterator<Item = &'static CommandMeta> {
    COMMANDS.iter().filter(move |m| m.group == group)
}

/// Fuzzy completion candidates: returns matches scored by quality (best first).
/// Falls back gracefully — prefix > contains > subsequence.
pub fn fuzzy_completion_candidates(
    partial: &str,
    score_fn: impl Fn(&str, &str) -> Option<usize>,
) -> Vec<(&'static str, &'static str)> {
    let mut scored: Vec<(usize, u32, bool, &'static str, &'static str)> = COMMANDS
        .iter()
        .filter_map(|m| {
            score_fn(m.name, partial).map(|s| {
                (
                    s.saturating_add(command_usage::usage_boost(m.name)),
                    command_usage::usage_count(m.name),
                    m.is_alias,
                    m.name,
                    m.description,
                )
            })
        })
        .collect();
    scored.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| b.1.cmp(&a.1))
            .then_with(|| a.2.cmp(&b.2))
            .then_with(|| a.3.cmp(b.3))
    });
    scored
        .into_iter()
        .map(|(_, _, _, name, desc)| (name, desc))
        .collect()
}

/// Get argument hint for a command (e.g., "/model" → "<name>").
pub fn get_arg_hint(command: &str) -> Option<&'static str> {
    COMMANDS
        .iter()
        .find(|m| m.name == command)
        .and_then(|m| m.arg_hint)
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::{
        COMMANDS, CommandGroup, TuiHandler, completion_candidates, fuzzy_completion_candidates,
        get_arg_hint, resolve_command, resolve_command_meta, subcommand_completions,
        suggest_commands,
    };
    use crate::cli::command_usage;

    #[test]
    fn all_commands_start_with_slash() {
        for meta in COMMANDS {
            assert!(
                meta.name.starts_with('/'),
                "command '{}' should start with /",
                meta.name
            );
        }
    }

    #[test]
    fn all_commands_have_descriptions() {
        for meta in COMMANDS {
            assert!(
                !meta.description.is_empty(),
                "command '{}' has empty description",
                meta.name
            );
        }
    }

    #[test]
    fn no_duplicate_command_names() {
        let mut names: Vec<_> = COMMANDS.iter().map(|m| m.name).collect();
        names.sort();
        let original_len = names.len();
        names.dedup();
        assert_eq!(
            names.len(),
            original_len,
            "duplicate command names detected"
        );
    }

    #[test]
    fn aliases_are_marked() {
        let quit = COMMANDS.iter().find(|m| m.name == "/quit");
        assert!(quit.is_some(), "/quit should exist");
        assert!(quit.unwrap().is_alias, "/quit should be marked as alias");
    }

    #[test]
    fn every_group_has_commands() {
        for group in CommandGroup::ALL {
            let count = COMMANDS.iter().filter(|m| m.group == *group).count();
            assert!(count > 0, "group {:?} has no commands", group);
        }
    }

    #[test]
    fn resolve_exact_match() {
        let result = resolve_command("/help");
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "/help");
    }

    #[test]
    fn resolve_prefix_unique() {
        let result = resolve_command("/hel");
        assert!(result.is_ok(), "got: {result:?}");
        assert_eq!(result.unwrap(), "/help");
    }

    #[test]
    fn resolve_prefix_ambiguous() {
        // Both /session and /sync start with /s
        let result = resolve_command("/s");
        assert!(result.is_err());
        let candidates = result.unwrap_err();
        assert!(candidates.len() > 1);
    }

    #[test]
    fn subcommand_completions_work() {
        let subs = subcommand_completions("/skill");
        assert!(subs.is_some());
        let subs = subs.unwrap();
        assert!(subs.iter().any(|(tok, _)| *tok == "browse"));
        assert!(subs.iter().any(|(tok, _)| *tok == "installed"));
        assert!(subs.iter().any(|(tok, _)| *tok == "list"));
        assert!(subs.iter().any(|(tok, _)| *tok == "info"));
        assert!(subs.iter().any(|(tok, _)| *tok == "publish"));
        assert!(subs.iter().any(|(tok, _)| *tok == "rollback"));
        assert!(subs.iter().any(|(tok, _)| *tok == "trending"));
        assert!(subs.iter().any(|(tok, _)| *tok == "uninstall"));
        assert!(subs.iter().any(|(tok, _)| *tok == "upgrade"));
    }

    #[test]
    fn allow_command_lists_accept_edits_mode() {
        let allow = COMMANDS
            .iter()
            .find(|meta| meta.name == "/allow")
            .expect("/allow command");
        assert!(allow.description.contains("accept_edits"));
        assert!(allow.description.contains("plan"));
        assert_eq!(
            allow.arg_hint,
            Some("[auto|accept_edits|plan|prompt|deny|rules|trust|untrust|trace]")
        );

        let subs = subcommand_completions("/allow").expect("/allow subcommands");
        assert!(subs.iter().any(|(tok, _)| *tok == "accept_edits"));
        assert!(!subs.iter().any(|(tok, _)| *tok == "accept-edits"));
        assert!(!subs.iter().any(|(tok, _)| *tok == "default"));
        assert!(!subs.iter().any(|(tok, _)| *tok == "ask"));
        assert!(!subs.iter().any(|(tok, _)| *tok == "all"));
        assert!(!subs.iter().any(|(tok, _)| *tok == "status"));
        assert!(subs.iter().any(|(tok, _)| *tok == "plan"));
        assert!(subs.iter().any(|(tok, _)| *tok == "trust"));
        assert!(subs.iter().any(|(tok, _)| *tok == "untrust"));
        assert!(subs.iter().any(|(tok, _)| *tok == "trace"));
    }

    #[test]
    fn session_subcommand_completions_include_runtime_tools() {
        let subs = subcommand_completions("/session");
        assert!(subs.is_some());
        let subs = subs.unwrap();
        assert!(subs.iter().any(|(tok, _)| *tok == "list"));
        assert!(subs.iter().any(|(tok, _)| *tok == "history"));
        assert!(subs.iter().any(|(tok, _)| *tok == "fork"));
        assert!(subs.iter().any(|(tok, _)| *tok == "analyze"));
        assert!(subs.iter().any(|(tok, _)| *tok == "export"));
    }

    #[test]
    fn suggest_finds_similar() {
        // Test prefix match - "/hel" should match "/help"
        let suggestions = suggest_commands("/hel", 5);
        assert!(
            suggestions.contains(&"/help"),
            "suggestions should include /help for prefix /hel"
        );
    }

    #[test]
    fn suggest_finds_fuzzy_typo() {
        let suggestions = suggest_commands("/hlp", 5);
        assert!(
            suggestions.contains(&"/help"),
            "suggestions should include /help for typo /hlp"
        );
    }

    #[test]
    #[serial_test::serial]
    fn completion_candidates_prioritize_frequently_used_commands() {
        let dir = tempfile::tempdir().unwrap();
        command_usage::set_test_dir(dir.path());
        command_usage::reset_for_tests();
        for _ in 0..6 {
            command_usage::record_command_use("/session").unwrap();
        }

        let rows = completion_candidates("/");
        assert_eq!(rows.first().map(|row| row.0), Some("/session"));

        command_usage::clear_test_dir();
        command_usage::reset_for_tests();
    }

    #[test]
    fn get_arg_hint_from_registry() {
        // Commands with arg_hint defined in registry
        assert_eq!(
            get_arg_hint("/model"),
            Some("[info | list | clear | <name>]")
        );
        assert_eq!(get_arg_hint("/undo"), Some("[N]"));
        assert_eq!(get_arg_hint("/resume"), Some("[session_id]"));

        // Commands with subcommands should also have arg hints
        assert!(get_arg_hint("/session").is_some());
        assert!(get_arg_hint("/skill").is_some());
        assert!(get_arg_hint("/team").is_some());

        // Command without arg_hint should return None
        assert!(get_arg_hint("/clear").is_none());
        assert!(get_arg_hint("/nonexistent").is_none());
    }

    #[test]
    fn fuzzy_completion_candidates_keep_aliases() {
        let rows = fuzzy_completion_candidates("/qit", |tok, partial| {
            if tok == "/quit" && partial == "/qit" {
                Some(1)
            } else {
                None
            }
        });
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, "/quit");
    }

    // ── resolve_command_meta / TuiHandler routing tests ──────────────────

    #[test]
    fn resolve_command_meta_returns_tui_handler_panel() {
        // /context is explicitly marked as Panel
        let meta = resolve_command_meta("/context").expect("should resolve /context");
        assert_eq!(meta.tui_handler, TuiHandler::Panel);
    }

    #[test]
    fn resolve_command_meta_returns_tui_handler_selector() {
        // /model is explicitly marked as Selector
        let meta = resolve_command_meta("/model").expect("should resolve /model");
        assert_eq!(meta.tui_handler, TuiHandler::Selector);
    }

    #[test]
    fn resolve_command_meta_returns_tui_handler_fallback() {
        // /clear is explicitly marked as Fallback
        let meta = resolve_command_meta("/clear").expect("should resolve /clear");
        assert_eq!(meta.tui_handler, TuiHandler::Fallback);
    }

    #[test]
    fn resolve_command_meta_returns_tui_handler_inline() {
        // /help is explicitly marked as Inline
        let meta = resolve_command_meta("/help").expect("should resolve /help");
        assert_eq!(meta.tui_handler, TuiHandler::Inline);
    }

    #[test]
    fn resolve_command_meta_uses_fallback_for_non_native_commands() {
        let meta = resolve_command_meta("/mcp").expect("should resolve /mcp");
        assert_eq!(meta.tui_handler, TuiHandler::Fallback);
    }

    #[test]
    fn resolve_command_meta_returns_none_for_unknown_command() {
        assert!(resolve_command_meta("/nonexistent_cmd_xyz").is_none());
    }

    #[test]
    fn task_slash_command_replaces_legacy_job_surface() {
        let task = resolve_command_meta("/task").expect("should resolve /task");
        assert_eq!(task.name, "/task");
        assert!(
            resolve_command_meta("/job").is_none(),
            "legacy /job must not remain registered"
        );
    }

    #[test]
    fn resolve_command_meta_prefix_match_also_resolves_handler() {
        // /reg prefixes to /register which is marked as Panel
        let meta = resolve_command_meta("/reg").expect("should resolve /reg → /register");
        assert_eq!(meta.name, "/register");
        assert_eq!(meta.tui_handler, TuiHandler::Panel);
    }

    #[test]
    fn info_and_aliases_are_discoverable() {
        let info = resolve_command_meta("/info").expect("should resolve /info");
        assert_eq!(info.tui_handler, TuiHandler::Panel);

        let whoami = resolve_command_meta("/whoami").expect("should resolve /whoami");
        assert!(whoami.is_alias, "/whoami should be treated as an alias");

        let health = resolve_command_meta("/health").expect("should resolve /health");
        assert!(health.is_alias, "/health should be treated as an alias");
    }

    #[test]
    fn fallback_commands_do_not_drop_into_chat_forward() {
        for cmd in [
            "/checkpoint",
            "/grep",
            "/diff",
            "/review",
            "/report",
            "/task",
            "/debug",
            "/lsp",
            "/telemetry",
            "/health",
            "/sync",
            "/rewind",
            "/mcp",
            "/team",
            "/agent",
            "/messaging",
            "/logout",
            "/profile",
            "/memory-setup",
            "/diagnostics",
            "/bug",
        ] {
            let meta = resolve_command_meta(cmd).unwrap_or_else(|| panic!("missing {cmd}"));
            assert_eq!(
                meta.tui_handler,
                TuiHandler::Fallback,
                "{cmd} should preserve slash execution instead of forwarding to chat"
            );
        }
    }

    #[test]
    fn all_handled_commands_have_explicit_handler_or_default_chat_forward() {
        // Sanity check: every command has a tui_handler set (ChatForward is the default)
        for cmd in COMMANDS {
            if cmd.is_alias {
                continue;
            }
            // All handlers are valid variants
            match cmd.tui_handler {
                TuiHandler::Panel
                | TuiHandler::Selector
                | TuiHandler::Inline
                | TuiHandler::Fallback
                | TuiHandler::ChatForward => {}
            }
        }
    }
}
