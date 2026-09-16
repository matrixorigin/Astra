//! Unified slash command registry — single source of truth for command metadata.
//!
//! This module consolidates:
//! - Command names and descriptions
//! - Group categorization
//! - Subcommand completions
//! - Argument hints
//!
//! All slash command metadata should be defined here. Other modules (TUI slash
//! menus, main.rs) should query this registry rather than maintaining their
//! own static arrays.

use crate::cli::command_usage;

/// Task-oriented groups for the TUI command browser and grouped popup.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CommandGroup {
    Common,
    Sessions,
    Work,
    Inspect,
    Tools,
    Settings,
    /// Line-mode workspace commands, intentionally hidden from TUI discovery.
    Workspace,
}

impl CommandGroup {
    /// All groups in display order.
    pub const ALL: &'static [CommandGroup] = &[
        CommandGroup::Common,
        CommandGroup::Sessions,
        CommandGroup::Work,
        CommandGroup::Inspect,
        CommandGroup::Tools,
        CommandGroup::Settings,
        CommandGroup::Workspace,
    ];

    /// Display title for this group.
    pub const fn title(&self) -> &'static str {
        match self {
            CommandGroup::Common => "Common",
            CommandGroup::Sessions => "Sessions",
            CommandGroup::Work => "Work",
            CommandGroup::Inspect => "Inspect",
            CommandGroup::Tools => "Tools",
            CommandGroup::Settings => "Settings",
            CommandGroup::Workspace => "Workspace",
        }
    }
}

/// Whether a slash command can be completed inside the workbench.
///
/// A command that is unavailable here is still valid for non-interactive
/// command-line use. It must never cause the workbench to tear itself down and
/// hand control to a different UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TuiCommandRoute {
    /// Handled without tearing down the TUI.
    Native,
    /// Has no truthful workbench action yet.
    Unavailable,
}

/// How prominently an action appears in interactive discovery surfaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandDiscoverability {
    /// Featured in command help and ranked first in the slash list.
    Primary,
    /// Available through the complete slash list and typed search, but not
    /// featured in command help or ranked ahead of other actions.
    SearchOnly,
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
    /// Subcommand completions (token, description) for Tab completion.
    pub subcommands: &'static [(&'static str, &'static str)],
    /// Workbench-specific completions. `None` means the command's complete
    /// CLI surface is also available in the TUI; `Some` prevents a native
    /// command from advertising subcommands that would leave the workbench or
    /// merely print an unavailable usage string.
    pub tui_subcommands: Option<&'static [(&'static str, &'static str)]>,
    /// Argument hint shown inline (e.g., "<name>").
    pub arg_hint: Option<&'static str>,
    /// How this command is delivered inside the TUI.
    pub tui_route: TuiCommandRoute,
    /// Whether this command is featured in curated command help and slash
    /// completion ordering.
    pub discoverability: CommandDiscoverability,
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
            subcommands: &[],
            tui_subcommands: None,
            arg_hint: None,
            tui_route: TuiCommandRoute::Unavailable,
            discoverability: CommandDiscoverability::SearchOnly,
            usage_examples: &[],
        }
    }

    /// Add subcommands for Tab completion.
    pub const fn with_subcommands(
        mut self,
        subcommands: &'static [(&'static str, &'static str)],
    ) -> Self {
        self.subcommands = subcommands;
        self
    }

    /// Narrow subcommand discovery to actions with a real in-workbench
    /// interaction. The command remains available to non-interactive CLI
    /// callers through [`Self::subcommands`].
    pub const fn with_tui_subcommands(
        mut self,
        subcommands: &'static [(&'static str, &'static str)],
    ) -> Self {
        self.tui_subcommands = Some(subcommands);
        self
    }

    pub const fn visible_tui_subcommands(&self) -> &'static [(&'static str, &'static str)] {
        match self.tui_subcommands {
            Some(subcommands) => subcommands,
            None => self.subcommands,
        }
    }

    /// Add argument hint.
    pub const fn with_arg_hint(mut self, hint: &'static str) -> Self {
        self.arg_hint = Some(hint);
        self
    }

    /// Set how this command is delivered inside the TUI.
    pub const fn with_tui_route(mut self, route: TuiCommandRoute) -> Self {
        self.tui_route = route;
        self
    }

    /// Promote this command into curated command help and the top of the
    /// slash completion list.
    pub const fn primary(mut self) -> Self {
        self.discoverability = CommandDiscoverability::Primary;
        self
    }

    pub const fn is_primary(&self) -> bool {
        matches!(self.discoverability, CommandDiscoverability::Primary)
    }

    /// Whether this action can be discovered and completed inside the TUI.
    pub const fn is_available_in_tui(&self) -> bool {
        matches!(self.tui_route, TuiCommandRoute::Native)
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

// The picker is the default `/model` action, so its `list` alias is accepted
// but does not need a second completion row.
const TUI_MODEL_SUBCOMMANDS: &[(&str, &str)] = &[
    ("info", "Show details for the current model"),
    ("clear", "Clear the active model selection"),
];

const STATS_SUBCOMMANDS: &[(&str, &str)] = &[
    ("cost", "Per-session API cost estimate"),
    ("health", "Tool health dashboard"),
    ("history", "Aggregate stats across recent sessions"),
    ("learn", "Learning insights: patterns, drift, exploration"),
    ("tools", "Tool performance: calls, timing, success rate"),
];

const EXPLAIN_SUBCOMMANDS: &[(&str, &str)] = &[
    ("on", "Show concise measured execution facts"),
    (
        "verbose",
        "Include context, dependency, and coverage details",
    ),
    (
        "off",
        "Hide explain output while retaining durable evidence",
    ),
];

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
];

// `/mcp` and `/mcp help` both show command guidance; `/mcp status` is an
// alias for `/mcp list`. Keep one discoverable entry for each distinct action.
const TUI_MCP_SUBCOMMANDS: &[(&str, &str)] = &[
    ("list", "Overview of connected servers and capabilities"),
    ("servers", "Server details and tool counts"),
    ("tools", "List callable tools, optionally for one server"),
    ("inspect", "Inspect a tool schema"),
    ("prompts", "List prompt templates"),
    ("resources", "List readable resources"),
    ("read", "Read one resource"),
    ("ping", "Check server connectivity"),
    ("history", "Recent MCP tool-call history"),
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

// The default `/memory` action lists memories. Hide that alias and expose only
// the other distinct actions with a complete workbench interaction.
const TUI_MEMORY_SUBCOMMANDS: &[(&str, &str)] = &[
    ("search", "Search remembered facts (needs a query)"),
    ("stats", "Open memory statistics"),
    ("health", "Open memory health"),
    ("session", "Show memory for this session"),
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

// Shared line-mode completions retain `/session fork` and `/session list`.
// The TUI uses the narrower list below to omit the unsupported fork action
// and the redundant picker alias.
const SESSION_SUBCOMMANDS: &[(&str, &str)] = &[
    ("analyze", "Counter-only diagnostics for a session"),
    ("export", "Write a markdown transcript to disk"),
    ("fork", "Branch a parallel session from a parent"),
    ("history", "Scroll a session's conversation history"),
    ("list", "Pick a session to resume"),
];

// `/session list` remains accepted as an alias for `/resume`; session fork is
// line-mode only. Surface only actions that add a distinct workbench flow.
const TUI_SESSION_SUBCOMMANDS: &[(&str, &str)] = &[
    ("analyze", "Show a concise session summary"),
    ("export", "Export a session transcript to Markdown"),
    ("history", "Open a session transcript"),
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
    (
        "auto",
        "Auto-approve normal tool risk; some git/sensitive gates may still stop",
    ),
    (
        "bypass",
        "Skip approval prompts; catastrophic and policy hard-denies still apply",
    ),
    (
        "read_only",
        "Read-only tool capability; use /plan to enter the planning workflow",
    ),
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

const CONFIG_SUBCOMMANDS: &[(&str, &str)] = &[("edit", "Open the runtime configuration editor")];

const HELP_SUBCOMMANDS: &[(&str, &str)] = &[("keys", "Keyboard shortcuts")];

// The bare command opens the agent workbench; `list` is only an alias.
const TUI_AGENT_SUBCOMMANDS: &[(&str, &str)] = &[];
// The bare command opens the Work board; `status` is only an alias.
const TUI_WORK_SUBCOMMANDS: &[(&str, &str)] = &[
    ("start", "Track this conversation as durable Work"),
    (
        "execution",
        "Show live execution placement and handoff targets",
    ),
];
// The bare command opens the editor; `edit` is only an alias.
const TUI_CONFIG_SUBCOMMANDS: &[(&str, &str)] = &[];
const WORK_SUBCOMMANDS: &[(&str, &str)] = &[
    ("start", "Track this conversation as durable Work"),
    ("status", "Open the canonical Work task board"),
];

// `/skill` itself opens the `$` skill browser. Do not advertise marketplace
// management commands in the TUI until they have a first-class native flow.
const TUI_SKILL_SUBCOMMANDS: &[(&str, &str)] = &[];

// ── The unified command registry ────────────────────────────────────────────

/// All registered slash commands.
pub static COMMANDS: &[CommandMeta] = &[
    // ── Common and session commands ───────────────────────────────────────
    CommandMeta::new(
        "/help",
        "Browse all workbench commands and keyboard shortcuts",
        CommandGroup::Common,
    )
    .with_subcommands(HELP_SUBCOMMANDS)
    .with_tui_route(TuiCommandRoute::Native)
    .primary(),
    CommandMeta::new(
        "/model",
        "Choose, inspect, or switch the active model",
        CommandGroup::Common,
    )
    .with_subcommands(MODEL_SUBCOMMANDS)
    .with_tui_subcommands(TUI_MODEL_SUBCOMMANDS)
    .with_arg_hint("[info | clear | <name>]")
    .with_tui_route(TuiCommandRoute::Native)
    .primary(),
    CommandMeta::new(
        "/clear",
        "Start a fresh conversation",
        CommandGroup::Sessions,
    )
    .with_tui_route(TuiCommandRoute::Native)
    .primary(),
    CommandMeta::new(
        "/undo",
        "Undo last turn(s): /undo [N]",
        CommandGroup::Sessions,
    )
    .with_arg_hint("[N]")
    .with_tui_route(TuiCommandRoute::Unavailable),
    CommandMeta::new(
        "/redo",
        "Redo undone turn(s): /redo [N]",
        CommandGroup::Sessions,
    )
    .with_arg_hint("[N]")
    .with_tui_route(TuiCommandRoute::Unavailable),
    CommandMeta::new(
        "/checkpoint",
        "Manual save: /checkpoint [label] — JSON + session md + journal",
        CommandGroup::Sessions,
    )
    .with_arg_hint("[label]")
    .with_tui_route(TuiCommandRoute::Unavailable),
    CommandMeta::new(
        "/history",
        "Open the complete conversation transcript",
        CommandGroup::Sessions,
    )
    .with_tui_route(TuiCommandRoute::Native)
    .primary(),
    CommandMeta::new(
        "/copy",
        "Copy the latest assistant response",
        CommandGroup::Common,
    )
    .with_tui_route(TuiCommandRoute::Native),
    CommandMeta::new(
        "/resume",
        "Continue a previous session",
        CommandGroup::Sessions,
    )
    .with_arg_hint("[session_id]")
    .with_tui_route(TuiCommandRoute::Native)
    .primary(),
    CommandMeta::new(
        "/timeline",
        "Browse this session's turn-by-turn activity",
        CommandGroup::Sessions,
    )
    .with_tui_route(TuiCommandRoute::Native),
    CommandMeta::new(
        "/worktrees",
        "Browse worktrees and their session counts",
        CommandGroup::Work,
    )
    .with_tui_route(TuiCommandRoute::Native),
    CommandMeta::new("/exit", "Exit Astra", CommandGroup::Common)
        .with_tui_route(TuiCommandRoute::Native),
    CommandMeta::new("/stop", "Stop the active run", CommandGroup::Work)
        .with_tui_route(TuiCommandRoute::Native)
        .primary(),
    // ── Workspace-only CLI commands ────────────────────────────────────────
    CommandMeta::new(
        "/grep",
        "Workspace ripgrep: <pattern> | files <glob> | review <pattern>",
        CommandGroup::Workspace,
    )
    .with_arg_hint("<pattern>")
    .with_tui_route(TuiCommandRoute::Unavailable),
    CommandMeta::new(
        "/diff",
        "Colored git diff (staged, stat, show <rev>, …)",
        CommandGroup::Workspace,
    )
    .with_subcommands(DIFF_SUBCOMMANDS)
    .with_arg_hint("[staged|unstaged|stat|show <rev>]")
    .with_tui_route(TuiCommandRoute::Unavailable),
    CommandMeta::new(
        "/review",
        "LLM review of git changes: /review [latest|<rev>|working]",
        CommandGroup::Workspace,
    )
    .with_subcommands(REVIEW_SUBCOMMANDS)
    .with_arg_hint("[latest|<rev>|working]")
    .with_tui_route(TuiCommandRoute::Unavailable),
    // ── Sessions and work ─────────────────────────────────────────────────
    CommandMeta::new(
        "/session",
        "View this session or open its history and details",
        CommandGroup::Sessions,
    )
    .with_subcommands(SESSION_SUBCOMMANDS)
    .with_tui_subcommands(TUI_SESSION_SUBCOMMANDS)
    .with_arg_hint("[action]")
    .with_tui_route(TuiCommandRoute::Native),
    CommandMeta::new(
        "/plan",
        "Enter or exit plan mode; then describe the plan",
        CommandGroup::Work,
    )
    .with_tui_route(TuiCommandRoute::Native)
    .primary(),
    CommandMeta::new(
        "/report",
        "Show the latest delivery report",
        CommandGroup::Work,
    )
    .with_tui_route(TuiCommandRoute::Unavailable),
    // ── Tools and durable work ────────────────────────────────────────────
    CommandMeta::new(
        "/memory",
        "Browse and search saved memories",
        CommandGroup::Tools,
    )
    .with_subcommands(MEMORY_SUBCOMMANDS)
    .with_tui_subcommands(TUI_MEMORY_SUBCOMMANDS)
    .with_arg_hint("[action]")
    .with_tui_route(TuiCommandRoute::Native),
    CommandMeta::new(
        "/work",
        "Open the Work board or start durable work",
        CommandGroup::Work,
    )
    .with_subcommands(WORK_SUBCOMMANDS)
    .with_tui_subcommands(TUI_WORK_SUBCOMMANDS)
    .with_arg_hint("[start <goal>]")
    .with_tui_route(TuiCommandRoute::Native)
    .primary(),
    CommandMeta::new(
        "/tasks",
        "Open the live background task panel",
        CommandGroup::Work,
    )
    .with_usage_examples(&["tasks"])
    .with_tui_route(TuiCommandRoute::Native),
    // ── Inspect and settings ───────────────────────────────────────────────
    CommandMeta::new(
        "/explain",
        "Show measured execution facts (on, verbose, or off)",
        CommandGroup::Inspect,
    )
    .with_subcommands(EXPLAIN_SUBCOMMANDS)
    .with_arg_hint("[on|verbose|off]")
    .with_usage_examples(&["explain", "explain verbose", "explain off"])
    .with_tui_route(TuiCommandRoute::Native),
    CommandMeta::new(
        "/compact",
        "Summarize & trim history (quick | no-memoria, …)",
        CommandGroup::Inspect,
    )
    .with_subcommands(COMPACT_SUBCOMMANDS)
    .with_arg_hint("[quick|no-memoria|summary-only]")
    .with_tui_route(TuiCommandRoute::Unavailable),
    CommandMeta::new(
        "/reflect",
        "Review session evidence with a read-only reflection",
        CommandGroup::Inspect,
    )
    .with_arg_hint("[topic | diff]")
    .with_tui_route(TuiCommandRoute::Native),
    CommandMeta::new(
        "/debug",
        "Developer-oriented session debugger: messages, tools, injections",
        CommandGroup::Inspect,
    )
    .with_tui_route(TuiCommandRoute::Unavailable),
    CommandMeta::new(
        "/cache",
        "Prompt-cache summary and diagnosis for the active session",
        CommandGroup::Inspect,
    )
    .with_arg_hint("[diagnosis|diag|detail]")
    .with_tui_route(TuiCommandRoute::Unavailable),
    CommandMeta::new(
        "/inspect",
        "Open the runtime inspector",
        CommandGroup::Inspect,
    )
    .with_tui_route(TuiCommandRoute::Native),
    CommandMeta::new(
        "/stats",
        "Explore session, tool, cost, health, and learning stats",
        CommandGroup::Inspect,
    )
    .with_subcommands(STATS_SUBCOMMANDS)
    .with_arg_hint("[subcommand]")
    .with_tui_route(TuiCommandRoute::Native),
    CommandMeta::new(
        "/lsp",
        "LSP backend status: /lsp [status]",
        CommandGroup::Inspect,
    )
    .with_arg_hint("[status]")
    .with_tui_route(TuiCommandRoute::Unavailable),
    CommandMeta::new(
        "/telemetry",
        "Deep observability traces: turns, drift, decisions, profile, context",
        CommandGroup::Inspect,
    )
    .with_tui_route(TuiCommandRoute::Unavailable),
    CommandMeta::new(
        "/config",
        "Edit runtime configuration",
        CommandGroup::Settings,
    )
    .with_subcommands(CONFIG_SUBCOMMANDS)
    .with_tui_subcommands(TUI_CONFIG_SUBCOMMANDS)
    .with_tui_route(TuiCommandRoute::Native),
    CommandMeta::new(
        "/sync",
        "Cloud sync status (server-owned)",
        CommandGroup::Inspect,
    )
    .with_subcommands(SYNC_SUBCOMMANDS)
    .with_arg_hint("[log|push|pull]")
    .with_tui_route(TuiCommandRoute::Unavailable),
    CommandMeta::new(
        "/context",
        "Inspect the context window or export a JSON snapshot",
        CommandGroup::Inspect,
    )
    .with_subcommands(&[("dump", "Write a JSON snapshot of the live context to disk")])
    .with_arg_hint("[dump [path]]")
    .with_tui_route(TuiCommandRoute::Native)
    .primary(),
    CommandMeta::new(
        "/rewind",
        "Rewind conversation to an earlier turn",
        CommandGroup::Sessions,
    )
    .with_arg_hint("<turn>")
    .with_tui_route(TuiCommandRoute::Unavailable),
    CommandMeta::new(
        "/info",
        "View version, session, model, permissions, and skill status",
        CommandGroup::Inspect,
    )
    .with_tui_route(TuiCommandRoute::Native),
    // ── Tool discovery ─────────────────────────────────────────────────────
    CommandMeta::new(
        "/skill",
        "Browse available skills and activate one",
        CommandGroup::Tools,
    )
    .with_subcommands(SKILL_SUBCOMMANDS)
    .with_tui_subcommands(TUI_SKILL_SUBCOMMANDS)
    .with_tui_route(TuiCommandRoute::Native),
    // ── Team and account commands ─────────────────────────────────────────
    CommandMeta::new(
        "/mcp",
        "Explore connected MCP servers, tools, prompts, and resources",
        CommandGroup::Tools,
    )
    .with_subcommands(MCP_SUBCOMMANDS)
    .with_tui_subcommands(TUI_MCP_SUBCOMMANDS)
    .with_arg_hint("[subcommand]")
    .with_usage_examples(&[
        "mcp list",
        "mcp tools",
        "mcp tools <server>",
        "mcp inspect <server>:<tool>",
        "mcp read <server>:<uri>",
        "mcp ping [server]",
    ])
    .with_tui_route(TuiCommandRoute::Native),
    // ── Team & account ───────────────────────────────────────────────────
    CommandMeta::new(
        "/team",
        "Teams: list|info|create|add-member|context|run|history|snapshot|restore|delete|help",
        CommandGroup::Work,
    )
    .with_subcommands(TEAM_SUBCOMMANDS)
    .with_arg_hint("[list|info|create|add-member|context|run|…]")
    .with_tui_route(TuiCommandRoute::Unavailable),
    CommandMeta::new(
        "/agent",
        "Open the agent monitor to inspect and manage runs",
        CommandGroup::Work,
    )
    .with_tui_subcommands(TUI_AGENT_SUBCOMMANDS)
    .with_tui_route(TuiCommandRoute::Native)
    .primary(),
    CommandMeta::new(
        "/messaging",
        "Inter-agent messaging: metrics, dlq, status",
        CommandGroup::Work,
    )
    .with_subcommands(MESSAGING_SUBCOMMANDS)
    .with_arg_hint("[metrics|dlq|status|help]")
    .with_tui_route(TuiCommandRoute::Unavailable),
    CommandMeta::new(
        "/login",
        "Sign in to your Astra account",
        CommandGroup::Settings,
    )
    .with_tui_route(TuiCommandRoute::Native),
    CommandMeta::new(
        "/register",
        "Create a new Astra account",
        CommandGroup::Settings,
    )
    .with_tui_route(TuiCommandRoute::Native),
    CommandMeta::new("/logout", "Logout from the API", CommandGroup::Settings)
        .with_tui_route(TuiCommandRoute::Unavailable),
    CommandMeta::new(
        "/profile",
        "Profile preferences: show, edit, scenario, stats, tools, experiments, reset",
        CommandGroup::Settings,
    )
    .with_subcommands(PROFILE_SUBCOMMANDS)
    .with_arg_hint("[show|edit <key> <value>|scenario|stats|tools|experiments|reset]")
    .with_tui_route(TuiCommandRoute::Unavailable),
    CommandMeta::new(
        "/memory-setup",
        "Guided Memoria configuration",
        CommandGroup::Settings,
    )
    .with_tui_route(TuiCommandRoute::Unavailable),
    // ── Settings and policy ────────────────────────────────────────────────
    CommandMeta::new(
        "/allow",
        "Choose a permission mode and manage workspace trust",
        CommandGroup::Settings,
    )
    .with_subcommands(ALLOW_SUBCOMMANDS)
    .with_arg_hint("[subcommand]")
    .with_tui_route(TuiCommandRoute::Native),
    CommandMeta::new(
        "/instructions",
        "View, reload, or disable project instructions",
        CommandGroup::Settings,
    )
    .with_subcommands(INSTRUCTIONS_SUBCOMMANDS)
    .with_arg_hint("[action]")
    .with_tui_route(TuiCommandRoute::Native),
    CommandMeta::new(
        "/diagnostics",
        "Binary, API, auth, environment checks",
        CommandGroup::Settings,
    )
    .with_arg_hint("— run all checks")
    .with_tui_route(TuiCommandRoute::Unavailable),
    // Note: /lsp is in Inspect, not duplicated here
    CommandMeta::new(
        "/bug",
        "Generate bug report: /bug [copy|save]",
        CommandGroup::Settings,
    )
    .with_arg_hint("[copy|save]")
    .with_tui_route(TuiCommandRoute::Unavailable),
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
    let mut scored: Vec<(usize, usize, &'static str)> = COMMANDS
        .iter()
        .map(|m| {
            (
                suggestion_score(m.name, input).saturating_add(command_usage::usage_boost(m.name)),
                m.name.len(),
                m.name,
            )
        })
        .filter(|(score, _, _)| *score > 0)
        .collect();
    scored.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| a.1.cmp(&b.1))
            .then_with(|| a.2.cmp(b.2))
    });
    scored
        .into_iter()
        .take(limit)
        .map(|(_, _, cmd)| cmd)
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
    // Prefer the user's frequent commands, then stable command-name order.
    rows.sort_by(|(a_name, _), (b_name, _)| {
        let a_usage = command_usage::usage_count(a_name);
        let b_usage = command_usage::usage_count(b_name);
        b_usage.cmp(&a_usage).then_with(|| a_name.cmp(b_name))
    });
    rows
}

/// Registered commands that have a complete native workbench interaction.
///
/// This is deliberately separate from `COMMANDS`: the registry also serves
/// non-interactive CLI entry points, while the TUI must not advertise actions
/// that would require a terminal handoff.
pub fn tui_commands() -> impl Iterator<Item = &'static CommandMeta> {
    COMMANDS
        .iter()
        .filter(|command| command.is_available_in_tui())
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
    tui_commands().filter(move |command| command.group == group)
}

/// Fuzzy completion candidates: returns matches scored by quality (best first).
/// Falls back gracefully — prefix > contains > subsequence.
pub fn fuzzy_completion_candidates(
    partial: &str,
    score_fn: impl Fn(&str, &str) -> Option<usize>,
) -> Vec<(&'static str, &'static str)> {
    let mut scored: Vec<(usize, u32, &'static str, &'static str)> = COMMANDS
        .iter()
        .filter_map(|m| {
            score_fn(m.name, partial).map(|s| {
                (
                    s.saturating_add(command_usage::usage_boost(m.name)),
                    command_usage::usage_count(m.name),
                    m.name,
                    m.description,
                )
            })
        })
        .collect();
    scored.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| b.1.cmp(&a.1))
            .then_with(|| a.2.cmp(b.2))
    });
    scored
        .into_iter()
        .map(|(_, _, name, desc)| (name, desc))
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
        COMMANDS, CommandGroup, TuiCommandRoute, completion_candidates,
        fuzzy_completion_candidates, get_arg_hint, resolve_command, resolve_command_meta,
        subcommand_completions, suggest_commands, tui_commands,
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
    fn duplicate_and_obsolete_entry_points_are_removed() {
        for command in [
            "/quit",
            "/whoami",
            "/health",
            "/panels",
            "/turn",
            "/verbose",
            "/experiment",
        ] {
            assert!(
                resolve_command_meta(command).is_none(),
                "removed command {command} must not remain in the action surface"
            );
        }
    }

    #[test]
    fn removed_tuning_command_is_not_registered() {
        assert!(
            COMMANDS.iter().all(|m| m.name != "/tuning"),
            "/tuning must not remain as a removed alias or deprecated command"
        );
        assert!(resolve_command_meta("/tuning").is_none());
        assert!(resolve_command("/tuning").is_err());
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
    fn workbench_subcommands_only_advertise_native_actions() {
        let agent = resolve_command_meta("/agent").expect("agent command registered");
        assert!(agent.visible_tui_subcommands().is_empty());

        let inspect = resolve_command_meta("/inspect").expect("inspect command registered");
        assert!(inspect.visible_tui_subcommands().is_empty());
        assert_eq!(inspect.arg_hint, None);

        let plan = resolve_command_meta("/plan").expect("plan command registered");
        assert_eq!(plan.arg_hint, None);

        let session = resolve_command_meta("/session").expect("session command registered");
        assert_eq!(
            session
                .visible_tui_subcommands()
                .iter()
                .map(|(name, _)| *name)
                .collect::<Vec<_>>(),
            vec!["analyze", "export", "history"]
        );
        assert_eq!(session.arg_hint, Some("[action]"));

        let model = resolve_command_meta("/model").expect("model command registered");
        assert_eq!(
            model
                .visible_tui_subcommands()
                .iter()
                .map(|(name, _)| *name)
                .collect::<Vec<_>>(),
            vec!["info", "clear"]
        );

        let work = resolve_command_meta("/work").expect("work command registered");
        assert_eq!(
            work.visible_tui_subcommands(),
            [
                ("start", "Track this conversation as durable Work"),
                (
                    "execution",
                    "Show live execution placement and handoff targets"
                )
            ]
        );

        let tasks = resolve_command_meta("/tasks").expect("tasks command registered");
        assert_eq!(tasks.tui_route, TuiCommandRoute::Native);
        assert!(tasks.visible_tui_subcommands().is_empty());
        assert_eq!(tasks.arg_hint, None);

        let config = resolve_command_meta("/config").expect("config command registered");
        assert!(config.visible_tui_subcommands().is_empty());
    }

    #[test]
    fn allow_command_lists_accept_edits_mode() {
        let allow = COMMANDS
            .iter()
            .find(|meta| meta.name == "/allow")
            .expect("/allow command");
        assert_eq!(allow.arg_hint, Some("[subcommand]"));

        let subs = subcommand_completions("/allow").expect("/allow subcommands");
        assert!(subs.iter().any(|(tok, _)| *tok == "bypass"));
        assert!(subs.iter().any(|(tok, _)| *tok == "accept_edits"));
        assert!(!subs.iter().any(|(tok, _)| *tok == "accept-edits"));
        assert!(!subs.iter().any(|(tok, _)| *tok == "default"));
        assert!(!subs.iter().any(|(tok, _)| *tok == "ask"));
        assert!(!subs.iter().any(|(tok, _)| *tok == "all"));
        assert!(!subs.iter().any(|(tok, _)| *tok == "status"));
        assert!(subs.iter().any(|(tok, _)| *tok == "read_only"));
        assert!(!subs.iter().any(|(tok, _)| *tok == "plan"));
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
        assert_eq!(get_arg_hint("/model"), Some("[info | clear | <name>]"));
        assert_eq!(get_arg_hint("/undo"), Some("[N]"));
        assert_eq!(get_arg_hint("/resume"), Some("[session_id]"));

        // Commands with subcommands should also have arg hints
        assert!(get_arg_hint("/session").is_some());
        assert!(get_arg_hint("/skill").is_none());
        assert!(get_arg_hint("/team").is_some());

        // Command without arg_hint should return None
        assert!(get_arg_hint("/clear").is_none());
        assert!(get_arg_hint("/nonexistent").is_none());
    }

    #[test]
    fn fuzzy_completion_candidates_find_canonical_commands() {
        let rows = fuzzy_completion_candidates("/modl", |tok, partial| {
            if tok == "/model" && partial == "/modl" {
                Some(1)
            } else {
                None
            }
        });
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, "/model");
    }

    // ── resolve_command_meta / TUI delivery tests ────────────────────────

    #[test]
    fn native_routes_cover_inline_panels_and_selectors_without_decorative_subtypes() {
        for command in [
            "/help", "/model", "/clear", "/stop", "/context", "/mcp", "/agent", "/reflect",
        ] {
            let meta = resolve_command_meta(command).unwrap_or_else(|| panic!("missing {command}"));
            assert_eq!(meta.tui_route, TuiCommandRoute::Native, "{command}");
        }
    }

    #[test]
    fn resolve_command_meta_marks_non_native_action_unavailable_in_tui() {
        let meta = resolve_command_meta("/undo").expect("should resolve /undo");
        assert_eq!(meta.tui_route, TuiCommandRoute::Unavailable);
        assert!(!meta.is_available_in_tui());
    }

    #[test]
    fn resolve_command_meta_returns_none_for_unknown_command() {
        assert!(resolve_command_meta("/nonexistent_cmd_xyz").is_none());
    }

    #[test]
    fn resolve_command_meta_prefix_match_also_resolves_route() {
        let meta = resolve_command_meta("/reg").expect("should resolve /reg → /register");
        assert_eq!(meta.name, "/register");
        assert_eq!(meta.tui_route, TuiCommandRoute::Native);
    }

    #[test]
    fn primary_action_surface_is_curated_and_native() {
        let info = resolve_command_meta("/info").expect("should resolve /info");
        assert_eq!(info.tui_route, TuiCommandRoute::Native);

        let names: Vec<_> = COMMANDS
            .iter()
            .filter(|command| command.is_primary())
            .map(|command| command.name)
            .collect();
        assert_eq!(
            names,
            vec![
                "/help", "/model", "/clear", "/history", "/resume", "/stop", "/plan", "/work",
                "/context", "/agent",
            ]
        );
        assert!(
            COMMANDS
                .iter()
                .filter(|command| command.is_primary())
                .all(|command| command.tui_route == TuiCommandRoute::Native)
        );
    }

    #[test]
    fn tui_discovery_exposes_only_actions_the_workbench_can_complete() {
        let commands: Vec<_> = tui_commands().collect();
        assert!(!commands.is_empty());
        assert!(commands.iter().all(|command| command.is_available_in_tui()));
        assert_eq!(
            commands
                .iter()
                .map(|command| command.name)
                .collect::<Vec<_>>(),
            vec![
                "/help",
                "/model",
                "/clear",
                "/history",
                "/copy",
                "/resume",
                "/timeline",
                "/worktrees",
                "/exit",
                "/stop",
                "/session",
                "/plan",
                "/memory",
                "/work",
                "/tasks",
                "/explain",
                "/reflect",
                "/inspect",
                "/stats",
                "/config",
                "/context",
                "/info",
                "/skill",
                "/mcp",
                "/agent",
                "/login",
                "/register",
                "/allow",
                "/instructions",
            ]
        );
        assert!(
            !commands.iter().any(|command| command.name == "/undo"),
            "a terminal-only command must not appear in the TUI command surface"
        );
    }

    #[test]
    fn tui_subcommand_discovery_shows_distinct_workbench_actions_only() {
        let memory = resolve_command_meta("/memory").expect("memory command");
        let memory_subcommands: Vec<_> = memory
            .visible_tui_subcommands()
            .iter()
            .map(|(name, _)| *name)
            .collect();
        assert_eq!(
            memory_subcommands,
            vec!["search", "stats", "health", "session"]
        );

        let skill = resolve_command_meta("/skill").expect("skill command");
        assert!(
            skill.visible_tui_subcommands().is_empty(),
            "the workbench skill browser is the only native skill action"
        );

        let mcp = resolve_command_meta("/mcp").expect("MCP command");
        let mcp_subcommands: Vec<_> = mcp
            .visible_tui_subcommands()
            .iter()
            .map(|(name, _)| *name)
            .collect();
        assert_eq!(
            mcp_subcommands,
            vec![
                "list",
                "servers",
                "tools",
                "inspect",
                "prompts",
                "resources",
                "read",
                "ping",
                "history",
            ]
        );

        let cli_session_subcommands = subcommand_completions("/session").unwrap();
        assert!(
            cli_session_subcommands
                .iter()
                .any(|(name, _)| *name == "fork")
        );
        let tui_session = resolve_command_meta("/session")
            .unwrap()
            .visible_tui_subcommands();
        assert!(
            tui_session.iter().all(|(name, _)| *name != "fork"),
            "line-mode fork must not appear in workbench completion"
        );
    }
}
