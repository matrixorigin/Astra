//! Unified slash command registry — single source of truth for command metadata.
//!
//! This module consolidates:
//! - Command names and descriptions
//! - Group categorization (with icons)
//! - Aliases
//! - Subcommand completions
//! - Argument hints
//!
//! All slash command metadata should be defined here. Other modules (repl_ui, main.rs)
//! should query this registry rather than maintaining their own static arrays.

use crate::command_usage;

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
}

// ── Subcommand completion arrays ────────────────────────────────────────────

const STATS_SUBCOMMANDS: &[(&str, &str)] = &[
    ("cost", "Per-session API cost estimate"),
    ("health", "Tool health dashboard"),
    ("history", "Aggregate stats across recent sessions"),
    ("learn", "Learning insights: patterns, drift, exploration"),
    ("tools", "Tool performance: calls, timing, success rate"),
];

const SYNC_SUBCOMMANDS: &[(&str, &str)] = &[
    ("log", "Recent sync event log"),
    ("pull", "Pull all domains from cloud"),
    ("push", "Force push dirty domains to cloud"),
];

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
    ("pin", "Pin skill to always load"),
    ("publish", "Publish to marketplace"),
    ("rollback", "Rollback installed skill version"),
    ("search", "Keyword search catalog"),
    ("stats", "Learning summary"),
    ("surfacing", "Agent catalog surfacing (dynamic/min/cap)"),
    ("system", "System skill helpers"),
    ("test", "Run skill test"),
    ("trending", "Show trending marketplace skills"),
    ("uninstall", "Remove local skill"),
    ("unpin", "Remove pinned skill"),
    ("upgrade", "Upgrade installed skill version"),
];

const MCP_SUBCOMMANDS: &[(&str, &str)] = &[
    ("add", "Add: /mcp add <name> <command> [args…]"),
    (
        "complete",
        "Completions: /mcp complete <server>:prompt:<name> <arg> [value]",
    ),
    ("log-level", "Set level: /mcp log-level <server> <level>"),
    ("ping", "Ping: /mcp ping [server]"),
    ("prompt", "Invoke: /mcp prompt <server>:<name> [args]"),
    ("prompts", "List available MCP prompts"),
    ("remove", "Remove: /mcp remove <name>"),
    ("resource", "Read: /mcp resource <server>:<uri>"),
    ("resources", "List available MCP resources"),
    ("servers", "Show server details and tools"),
    ("status", "Show connection status table"),
    ("subscribe", "Subscribe: /mcp subscribe <server>:<uri>"),
    (
        "unsubscribe",
        "Unsubscribe: /mcp unsubscribe <server>:<uri>",
    ),
];

const PLAN_SUBCOMMANDS: &[(&str, &str)] = &[
    ("exit", "Exit structured plan mode"),
    ("go", "Execute plan (auto mode)"),
    ("help", "Show all plan commands"),
    ("pause", "Pause plan execution"),
    ("resume", "Resume plan execution"),
    ("show", "Display current plan"),
    ("status", "Show plan progress"),
    ("step", "Execute plan (step-by-step)"),
];

const TASK_SUBCOMMANDS: &[(&str, &str)] = &[
    ("add", "Create task (needs title)"),
    ("done", "Mark task done (needs id/query)"),
    ("list", "List tasks"),
    ("run", "Run task prompt"),
    ("result", "Task result (needs id)"),
    ("status", "Task status (needs id/query)"),
];

const MEMORY_SUBCOMMANDS: &[(&str, &str)] = &[
    ("inspect", "Inspect memory entry (needs id)"),
    ("list", "List memories"),
    ("search", "Search memories (needs query)"),
];

const SESSION_SUBCOMMANDS: &[(&str, &str)] = &[
    ("analyze", "Deep session diagnostics"),
    ("cleanup", "Clean stale sessions"),
    ("context", "Show context assembly trace"),
    ("drift", "Inspect session drift signals"),
    ("errors", "Session errors"),
    ("export", "Export session"),
    ("fork", "Fork session"),
    ("history", "Session conversation history"),
    ("list", "List journals"),
    ("trace", "Toggle per-session full LLM capture"),
    ("verify", "Verify session integrity"),
];

const DIFF_SUBCOMMANDS: &[(&str, &str)] = &[
    ("help", "Diff usage"),
    ("patch", "Unstaged diff alias"),
    ("show", "git show <rev> (needs rev)"),
    ("staged", "Staged vs HEAD"),
    ("stat", "Diff stat vs HEAD"),
    ("unstaged", "Unstaged only"),
];

const TURN_SUBCOMMANDS: &[(&str, &str)] = &[("list", "List all journal turns")];

const EXPERIMENT_SUBCOMMANDS: &[(&str, &str)] = &[
    ("analyze", "Analyze experiment results"),
    ("create", "Create new experiment"),
    ("list", "List all experiments"),
    ("show", "Show experiment details"),
    ("start", "Start an experiment"),
    ("status", "Show active experiment"),
    ("stop", "Stop an experiment"),
];

const STYLE_SUBCOMMANDS: &[(&str, &str)] = &[
    ("colorful", "Colorful theme"),
    ("default", "Default theme"),
    ("high-contrast", "High-contrast theme"),
    ("list", "List available themes"),
    ("minimal", "Minimal theme"),
];

const ALLOW_SUBCOMMANDS: &[(&str, &str)] = &[
    ("all", "Auto-approve all (alias for auto)"),
    ("auto", "Auto-approve all tool use"),
    ("deny", "Deny all tool use"),
    ("prompt", "Prompt before tool use"),
    ("rules", "Show current permission rules"),
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

const TUNING_SUBCOMMANDS: &[(&str, &str)] = &[
    ("config", "Show tuning configuration"),
    ("history", "Show tuning history"),
    ("reset", "Reset tuning state"),
    ("status", "Show tuning status (default)"),
];

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
    .with_subcommands(HELP_SUBCOMMANDS),
    CommandMeta::new(
        "/model",
        "List models or set active: /model <name>",
        CommandGroup::Core,
    )
    .with_arg_hint("<name>"),
    CommandMeta::new("/clear", "Start a new session", CommandGroup::Core),
    CommandMeta::new("/undo", "Undo last turn(s): /undo [N]", CommandGroup::Core)
        .with_arg_hint("[N]"),
    CommandMeta::new(
        "/redo",
        "Redo undone turn(s): /redo [N]",
        CommandGroup::Core,
    )
    .with_arg_hint("[N]"),
    CommandMeta::new(
        "/checkpoint",
        "Manual save: /checkpoint [label] — JSON + session md + journal",
        CommandGroup::Core,
    )
    .with_arg_hint("[label]"),
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
    .with_arg_hint("[session_id]"),
    CommandMeta::new("/exit", "Exit the REPL", CommandGroup::Core),
    CommandMeta::new(
        "/quit",
        "Exit the REPL (alias for /exit)",
        CommandGroup::Core,
    )
    .alias(),
    // ── Workspace ─────────────────────────────────────────────────────────
    CommandMeta::new(
        "/grep",
        "Workspace ripgrep: <pattern> | files <glob> | review <pattern>",
        CommandGroup::Workspace,
    )
    .with_arg_hint("<pattern>"),
    CommandMeta::new(
        "/diff",
        "Colored git diff (staged, stat, show <rev>, …)",
        CommandGroup::Workspace,
    )
    .with_subcommands(DIFF_SUBCOMMANDS)
    .with_arg_hint("[staged|unstaged|stat|show <rev>]"),
    CommandMeta::new(
        "/review",
        "LLM review of git changes: /review [latest|<rev>|working]",
        CommandGroup::Workspace,
    )
    .with_subcommands(REVIEW_SUBCOMMANDS)
    .with_arg_hint("[latest|<rev>|working]"),
    // ── Session & plan ───────────────────────────────────────────────────
    CommandMeta::new(
        "/session",
        "Session: history|errors|export|fork|list|cleanup|verify",
        CommandGroup::SessionPlan,
    )
    .with_subcommands(SESSION_SUBCOMMANDS)
    .with_arg_hint("[history|errors|export|fork|list|cleanup|verify]"),
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
        "Structured plan: go|step|pause|resume|exit|show|help",
        CommandGroup::SessionPlan,
    )
    .with_subcommands(PLAN_SUBCOMMANDS)
    .with_arg_hint("[go|step|pause|resume|exit|show|help]"),
    CommandMeta::new(
        "/plan go",
        "Execute plan (auto mode)",
        CommandGroup::SessionPlan,
    ),
    CommandMeta::new(
        "/plan step",
        "Execute plan (step-by-step)",
        CommandGroup::SessionPlan,
    ),
    CommandMeta::new(
        "/plan status",
        "Plan progress and state",
        CommandGroup::SessionPlan,
    ),
    CommandMeta::new(
        "/plan show",
        "Display current plan",
        CommandGroup::SessionPlan,
    ),
    CommandMeta::new(
        "/plan pause",
        "Pause plan execution",
        CommandGroup::SessionPlan,
    ),
    CommandMeta::new(
        "/plan resume",
        "Resume plan execution",
        CommandGroup::SessionPlan,
    ),
    CommandMeta::new("/plan exit", "Leave plan mode", CommandGroup::SessionPlan),
    CommandMeta::new(
        "/plan help",
        "Show all plan commands",
        CommandGroup::SessionPlan,
    ),
    CommandMeta::new(
        "/report",
        "Last delivery report (/report save = JSON)",
        CommandGroup::SessionPlan,
    ),
    // ── Memory & tasks ────────────────────────────────────────────────────
    CommandMeta::new(
        "/memory",
        "Memoria: list, search <q>, inspect <id>, …",
        CommandGroup::MemoryTasks,
    )
    .with_subcommands(MEMORY_SUBCOMMANDS)
    .with_arg_hint("[list|search <q>|inspect <id>]"),
    CommandMeta::new(
        "/task",
        "Tasks: list, add, done, status, run <prompt>, result <id>",
        CommandGroup::MemoryTasks,
    )
    .with_subcommands(TASK_SUBCOMMANDS)
    .with_arg_hint("[list|add <title>|done <id>|status <id>|run <prompt>|result <id>]"),
    // ── Observability ─────────────────────────────────────────────────────
    CommandMeta::new(
        "/explain",
        "Cycle explain: off → on (API) → verbose (+stderr)",
        CommandGroup::Observability,
    ),
    CommandMeta::new(
        "/verbose",
        "Verbose streaming on",
        CommandGroup::Observability,
    ),
    CommandMeta::new(
        "/compact",
        "Summarize & trim history (quick | no-memoria, …)",
        CommandGroup::Observability,
    )
    .with_subcommands(COMPACT_SUBCOMMANDS)
    .with_arg_hint("[quick|no-memoria|summary-only]"),
    CommandMeta::new(
        "/reflect",
        "Reflect on session (modes: skill_failure, performance, …)",
        CommandGroup::Observability,
    ),
    CommandMeta::new(
        "/turn",
        "Turn trace: /turn | list | N | seq:N | #N | id:N | @N | -1",
        CommandGroup::Observability,
    )
    .with_subcommands(TURN_SUBCOMMANDS),
    CommandMeta::new(
        "/debug",
        "Interactive session inspector (messages, tools, injections)",
        CommandGroup::Observability,
    ),
    CommandMeta::new(
        "/stats",
        "Session analytics: /stats [history|tools|cost|health|learn]",
        CommandGroup::Observability,
    )
    .with_subcommands(STATS_SUBCOMMANDS)
    .with_arg_hint("[cost|health|history|learn|tools]"),
    CommandMeta::new(
        "/lsp",
        "LSP backend status: /lsp [status]",
        CommandGroup::Observability,
    )
    .with_arg_hint("[status]"),
    CommandMeta::new(
        "/telemetry",
        "Session telemetry: turns, drift, decisions, profile",
        CommandGroup::Observability,
    ),
    CommandMeta::new(
        "/tuning",
        "Auto-tuning: status, history, config, reset",
        CommandGroup::Observability,
    )
    .with_subcommands(TUNING_SUBCOMMANDS)
    .with_arg_hint("[status|history|config|reset]"),
    CommandMeta::new(
        "/config",
        "Runtime config: show|paths|sources|diff|export [path]",
        CommandGroup::Observability,
    )
    .with_subcommands(CONFIG_SUBCOMMANDS)
    .with_arg_hint("[show|paths|sources|diff|export [path]]"),
    CommandMeta::new(
        "/sync",
        "Cloud sync status and push",
        CommandGroup::Observability,
    )
    .with_subcommands(SYNC_SUBCOMMANDS)
    .with_arg_hint("[log|push|pull]"),
    CommandMeta::new(
        "/context",
        "Context window / budget summary",
        CommandGroup::Observability,
    )
    .with_subcommands(&[
        ("breakdown", "Per-component token breakdown for last turn"),
        (
            "cognition",
            "Cognitive runtime flags (boosted/widen, recent tools, pending proposal)",
        ),
    ]),
    CommandMeta::new(
        "/rewind",
        "Rewind conversation to an earlier turn",
        CommandGroup::Observability,
    )
    .with_arg_hint("<turn>"),
    CommandMeta::new("/version", "Version info", CommandGroup::Observability),
    CommandMeta::new(
        "/whoami",
        "Agent self-awareness: identity, session, skills, pending proposals",
        CommandGroup::Observability,
    ),
    CommandMeta::new(
        "/experiment",
        "A/B testing: list, start, stop, analyze experiments",
        CommandGroup::Observability,
    )
    .with_subcommands(EXPERIMENT_SUBCOMMANDS)
    .with_arg_hint("[list|create|show|start|stop|status|analyze] [name]"),
    // ── Skills ────────────────────────────────────────────────────────────
    CommandMeta::new(
        "/skill",
        "Skills: browse|list|info|install|publish|search|new|test|dev|system|…",
        CommandGroup::Skills,
    )
    .with_subcommands(SKILL_SUBCOMMANDS)
    .with_arg_hint("[browse|list|info|install|publish|search|new|test|dev|feedback|…]"),
    // ── MCP ───────────────────────────────────────────────────────────────
    CommandMeta::new(
        "/mcp",
        "MCP: status|servers|prompts|resources|add|remove|ping|complete|…",
        CommandGroup::Mcp,
    )
    .with_subcommands(MCP_SUBCOMMANDS)
    .with_arg_hint("[status|servers|prompts|resources|add|remove|ping|…]"),
    // ── Team & account ───────────────────────────────────────────────────
    CommandMeta::new(
        "/team",
        "Teams: list|info|create|add-member|context|run|history|snapshot|restore|delete|help",
        CommandGroup::TeamAccount,
    )
    .with_subcommands(TEAM_SUBCOMMANDS)
    .with_arg_hint("[list|info|create|add-member|context|run|…]"),
    CommandMeta::new(
        "/agent",
        "Spawned agents: list, status, stop, logs",
        CommandGroup::TeamAccount,
    )
    .with_subcommands(AGENT_SUBCOMMANDS)
    .with_arg_hint("[list|status|stop|logs|help]"),
    CommandMeta::new(
        "/messaging",
        "Inter-agent messaging: metrics, dlq, status",
        CommandGroup::TeamAccount,
    )
    .with_subcommands(MESSAGING_SUBCOMMANDS)
    .with_arg_hint("[metrics|dlq|status|help]"),
    CommandMeta::new(
        "/login",
        "Authenticate with the API",
        CommandGroup::TeamAccount,
    ),
    CommandMeta::new(
        "/register",
        "Register a new account",
        CommandGroup::TeamAccount,
    ),
    CommandMeta::new("/logout", "Logout from the API", CommandGroup::TeamAccount),
    CommandMeta::new(
        "/memory-setup",
        "Guided Memoria configuration",
        CommandGroup::TeamAccount,
    ),
    // ── System ────────────────────────────────────────────────────────────
    CommandMeta::new(
        "/allow",
        "Permission mode: /allow [auto|prompt|deny|all|rules]",
        CommandGroup::System,
    )
    .with_subcommands(ALLOW_SUBCOMMANDS)
    .with_arg_hint("[auto|prompt|deny|all|rules]"),
    CommandMeta::new(
        "/yolo",
        "Auto-approve all tools (alias for /allow auto)",
        CommandGroup::System,
    )
    .alias(),
    CommandMeta::new(
        "/instructions",
        "Project instructions: /instructions [show|reload|off]",
        CommandGroup::System,
    )
    .with_subcommands(INSTRUCTIONS_SUBCOMMANDS)
    .with_arg_hint("[show|reload|off]"),
    CommandMeta::new(
        "/style",
        "Output theme: default | minimal | colorful | high-contrast",
        CommandGroup::System,
    )
    .with_subcommands(STYLE_SUBCOMMANDS)
    .with_arg_hint("[list|default|minimal|colorful|high-contrast]"),
    CommandMeta::new(
        "/diagnostics",
        "Binary, API, auth, environment checks",
        CommandGroup::System,
    )
    .with_arg_hint("— run all checks"),
    // Note: /lsp is in Observability group, not duplicated here
    CommandMeta::new(
        "/bug",
        "Generate bug report: /bug [copy|save]",
        CommandGroup::System,
    )
    .with_arg_hint("[copy|save]"),
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

/// Get all commands as (name, description) tuples for backward compatibility.
pub fn command_tuples() -> Vec<(&'static str, &'static str)> {
    COMMANDS.iter().map(|m| (m.name, m.description)).collect()
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
    use super::*;

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

        let yolo = COMMANDS.iter().find(|m| m.name == "/yolo");
        assert!(yolo.is_some(), "/yolo should exist");
        assert!(yolo.unwrap().is_alias, "/yolo should be marked as alias");
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
        assert!(subs.iter().any(|(tok, _)| *tok == "unpin"));
        assert!(subs.iter().any(|(tok, _)| *tok == "upgrade"));
    }

    #[test]
    fn session_subcommand_completions_include_runtime_tools() {
        let subs = subcommand_completions("/session");
        assert!(subs.is_some());
        let subs = subs.unwrap();
        assert!(subs.iter().any(|(tok, _)| *tok == "context"));
        assert!(subs.iter().any(|(tok, _)| *tok == "drift"));
        assert!(subs.iter().any(|(tok, _)| *tok == "trace"));
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
    fn command_tuples_compatibility() {
        let tuples = command_tuples();
        assert!(
            tuples.iter().any(|(cmd, _)| *cmd == "/help"),
            "tuples should contain /help"
        );
    }

    #[test]
    fn get_arg_hint_from_registry() {
        // Commands with arg_hint defined in registry
        assert_eq!(get_arg_hint("/model"), Some("<name>"));
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
}
