# Slash Commands Reference

Complete reference for slash commands available in the `astra` TUI.

## Overview

astra supports dozens of slash commands organized into 9 groups. Type `/help` to see all available commands, or `/help keys` for keyboard shortcuts.

## Command Groups

| Icon | Group          | Description                                       |
| ---- | -------------- | ------------------------------------------------- |
| ⚡   | Core           | Essential commands (help, model, session control) |
| 📂   | Workspace      | Code search, diff, and review                     |
| 🔭   | Observability  | Debugging, stats, and telemetry                   |
| 📋   | Session & Plan | Session management and structured planning        |
| 🧠   | Memory & Tasks | Memoria integration and task management           |
| 📦   | Skills         | Skill management and marketplace                  |
| 🔌   | MCP            | Model Context Protocol servers                    |
| 👥   | Team & Account | Multi-agent teams and authentication              |
| 🔧   | System         | Permissions, style, and diagnostics               |

---

## ⚡ Core Commands

### `/help [keys]`

Show available commands and usage hints.

```
/help        # Show command palette
/help keys   # Show keyboard shortcuts
```

### `/model [subcommand]`

Open the model picker, inspect the current model, or switch directly.

| Subcommand       | Description                |
| ---------------- | -------------------------- |
| (none) or `list` | Open the model picker      |
| `info`           | Show current model details |
| `clear`          | Reset to the API default   |
| `<name>`         | Switch directly to a model |

```
/model                      # Open picker
/model info                 # Inspect current model
/model claude-sonnet-4.6    # Switch directly
```

### `/clear`

Start a fresh session (clears conversation history).

### `/undo [N]`

Undo the last N turns (default: 1).

```
/undo      # Undo last turn
/undo 3    # Undo last 3 turns
```

### `/checkpoint [label]`

Save a manual checkpoint with optional label.

```
/checkpoint                     # Auto-labeled checkpoint
/checkpoint "before refactor"   # Labeled checkpoint
```

### `/history`

Display conversation turns. Supports in-memory grep.

```
/history           # Show all turns
/history grep foo  # Filter turns containing "foo"
```

### `/copy`

Copy the last assistant response to clipboard.

### `/resume [session_id]`

Resume a previous session.

```
/resume                      # Show recent sessions to pick
/resume abc123-def456        # Resume specific session
```

### `/exit`, `/quit`

Exit astra.

---

## 📂 Workspace Commands

### `/grep <pattern>`

Search workspace using ripgrep.

```
/grep "fn main"              # Search for pattern
/grep files "*.rs"           # List files matching glob
/grep review "TODO"          # Search with LLM review
```

### `/diff [subcommand]`

Show git diffs with syntax highlighting.

| Subcommand   | Description            |
| ------------ | ---------------------- |
| (none)       | Show unstaged changes  |
| `staged`     | Staged vs HEAD         |
| `stat`       | Diff stat summary      |
| `show <rev>` | Show specific revision |
| `patch`      | Alias for unstaged     |

```
/diff                # Unstaged changes
/diff staged         # What's about to be committed
/diff show HEAD~2    # Show specific commit
```

### `/review [latest|working|<rev>]`

Request LLM review of git changes.

```
/review              # Review HEAD commit
/review working      # Review working tree
/review HEAD~3       # Review specific revision
```

---

## 📋 Session & Plan Commands

### `/session [subcommand]`

Session management.

| Subcommand | Description                        |
| ---------- | ---------------------------------- |
| `history`  | Journal-style conversation history |
| `errors`   | Show session errors                |
| `export`   | Export to Markdown in cwd          |
| `fork`     | Fork session for experiments       |
| `list`     | List all sessions                  |
| `cleanup`  | Clean stale sessions               |
| `verify`   | Verify session integrity           |

```
/session list                    # List all
/session cleanup --days 7        # Clean old sessions
/session export                  # Export to Markdown
```

### `/plan [description]`

Structured planning mode for complex tasks.

| Form                  | Description                                              |
| --------------------- | -------------------------------------------------------- |
| `/plan`               | Enter plan mode, or leave it if already in `plan>`       |
| `/plan <description>` | Enter plan mode and immediately start planning that goal |

```
/plan Build a REST API           # Start planning
/plan                            # Enter or leave plan mode
```

Inside `plan>` mode, use plain commands like `execute`, `step`, `status`, `show`,
`pause`, `resume`, `cancel`, and `help`.

### `/report [save]`

Show the last delivery report from plan execution.

```
/report        # Display report
/report save   # Save as JSON
```

---

## 🧠 Memory & Tasks Commands

### `/memory [subcommand]`

Memoria memory operations. In the TUI, `list`, `search`, and `stats` open panels; `health` opens a read-only info pane; richer subcommands stay text-first.

| Subcommand                    | Description                          |
| ----------------------------- | ------------------------------------ |
| `list` / `ls`                 | List memories                        |
| `search <q>`                  | Search memories by content           |
| `show <id>`                   | Inspect specific memory              |
| `inspect <id>`                | Alias for `show <id>`                |
| `stats`                       | Count memories by type               |
| `dismiss <q>`                 | Lower retrieval score for matches    |
| `forget <id> --reason <text>` | Permanently delete a memory          |
| `session`                     | Show current session memory          |
| `health`                      | Show memory hygiene status           |
| `help`                        | Show the full `/memory` help surface |

```
/memory list                     # List all
/memory search "auth pattern"    # Search memories
/memory stats                    # Count memories by type
/memory show mem_abc123          # Inspect by ID
/memory session                  # Show current session memory
```

### `/task [subcommand]`

Task management for async work.

| Subcommand     | Description     |
| -------------- | --------------- |
| `list`         | List tasks      |
| `add <title>`  | Create task     |
| `done <id>`    | Mark complete   |
| `status <id>`  | Check status    |
| `run <prompt>` | Run task prompt |
| `result <id>`  | Get task result |

```
/task list                       # List tasks
/task add "Review PR #123"       # Create task
/task done review-pr-123         # Complete task
```

---

## 🔭 Observability Commands

Use these as:

1. `/stats` for operator-facing analytics and health.
2. `/inspect` for harness snapshots and exports.
3. `/telemetry` for deep observability traces.
4. `/debug` for developer-oriented low-level inspection.

### `/explain`

Cycle through explanation modes: off → on (API) → verbose (+stderr).

### `/verbose` _(removed)_

Migration: use `/stats` for metrics and `/timeline` for turn traces.

### `/compact [mode]`

Summarize and trim conversation history.

| Mode           | Description                     |
| -------------- | ------------------------------- |
| (none)         | Standard compaction             |
| `quick`        | Fast compaction without summary |
| `no-memoria`   | Compact without Memoria         |
| `summary-only` | Summarize without trimming      |

### `/reflect [topic[/facet]] [depth]`

Reflect on session observations through the observation-plane surface.

Examples:

```
/reflect
/reflect execution/errors diagnostic
/reflect execution/trace forensic
```

### `/turn` _(removed)_

Migration: use `/timeline` and press Enter to drill into a turn.

### `/debug`

Developer-oriented low-level inspection for messages, tools, and context injections.

### `/inspect [subcommand]`

Harness inspection utilities. Today these open the text fallback view directly.

| Subcommand  | Description                  |
| ----------- | ---------------------------- |
| (none)      | Show inspect help / overview |
| `budget`    | Token budget breakdown       |
| `tools`     | Tool dashboard               |
| `context`   | Context snapshot             |
| `json`      | Raw snapshot JSON            |
| `diff`      | Session state diff           |
| `history`   | Recent turn history          |
| `trace`     | Permission trace             |
| `forensics` | Forensics dump               |
| `export`    | Export inspect output        |

### `/stats [subcommand]`

Session analytics.

| Subcommand | Description                     |
| ---------- | ------------------------------- |
| (none)     | Current session stats           |
| `cost`     | API cost estimate               |
| `health`   | Tool health dashboard           |
| `history`  | Aggregate stats across sessions |
| `learn`    | Learning insights               |
| `tools`    | Tool performance metrics        |

### `/health [detail]`

Alias for `/stats health`. Use `detail` for the per-tool breakdown.

### `/config [subcommand]`

Runtime configuration inspection and editing.

| Subcommand | Description                                 |
| ---------- | ------------------------------------------- |
| (none)     | Open the interactive config editor panel    |
| `edit`     | Explicit alias for opening the editor panel |
| `show`     | Print the current config                    |
| `paths`    | Show config file locations                  |
| `sources`  | Show where each value came from             |
| `diff`     | Show differences from defaults              |
| `export`   | Export config to a file or stdout           |

### `/lsp [status]`

LSP (Language Server Protocol) backend status.

### `/telemetry [subcommand]`

Deep observability traces: turns, drift, decisions, profile, and context.

### `/sync [subcommand]`

Cloud sync status hint. Sync orchestration is server-owned; the CLI no longer opens MatrixOne connections or forces sync domains directly.

| Subcommand | Description                            |
| ---------- | -------------------------------------- |
| (none)     | Explain server-owned sync status       |
| `log`      | Point to server/API diagnostics        |
| `push`     | Deprecated; no direct CLI DB operation |
| `pull`     | Deprecated; no direct CLI DB operation |

### `/context`

Show context window and token budget summary.

### `/rewind <turn>`

Rewind conversation to an earlier turn.

### `/version`

Display version information.

### `/info`, `/whoami`

System and session identity at a glance: version, session, model, permissions, loaded skills, pending improvements, and recent tools.

---

## 📦 Skills Commands

### `/skill [subcommand]`

Skill management and marketplace.

| Subcommand       | Description                 |
| ---------------- | --------------------------- |
| `list [filter]`  | List skills                 |
| `info <name>`    | Skill details               |
| `search <q>`     | Keyword search              |
| `browse`         | Browse marketplace          |
| `install <name>` | Install from marketplace    |
| `new <name>`     | Create new skill            |
| `test <name>`    | Run skill test              |
| `dev <name>`     | Enter dev mode              |
| `health`         | Catalog health              |
| `create`         | Generate skill from session |
| `system`         | System skill helpers        |
| `stats`          | Learning summary            |

```
/skill list                      # List all skills
/skill browse                    # Browse marketplace
/skill install kubernetes        # Install skill
/skill new my-skill              # Scaffold new skill
/skill dev my-skill              # Enter dev mode
```

---

## 🔌 MCP Commands

### `/mcp [subcommand]`

Model Context Protocol server management.

| Subcommand                   | Description              |
| ---------------------------- | ------------------------ |
| `status`                     | Connection status table  |
| `servers`                    | Server details and tools |
| `prompts`                    | List available prompts   |
| `resources`                  | List available resources |
| `add <name> <cmd>`           | Add MCP server           |
| `remove <name>`              | Remove server            |
| `ping [server]`              | Ping server(s)           |
| `prompt <server>:<name>`     | Invoke prompt            |
| `resource <server>:<uri>`    | Read resource            |
| `subscribe <server>:<uri>`   | Subscribe to resource    |
| `unsubscribe <server>:<uri>` | Unsubscribe              |
| `log-level <server> <level>` | Set log level            |
| `complete <ref> <arg>`       | Get completions          |

```
/mcp status                                           # Status table
/mcp add github npx @modelcontextprotocol/server-github
/mcp servers                                          # Details
/mcp prompt github:search_repos "rust cli"            # Invoke
```

---

## 👥 Team & Account Commands

### `/team [subcommand]`

Multi-agent team management.

| Subcommand                  | Description        |
| --------------------------- | ------------------ |
| `list`                      | List teams         |
| `info <name>`               | Team information   |
| `create <name>`             | Create team        |
| `add-member <team> <agent>` | Add member         |
| `context <team> <ctx>`      | Set shared context |
| `run <team> <task>`         | Run task with team |
| `history <team>`            | Execution history  |
| `snapshot <team>`           | Save snapshot      |
| `restore <team> <snap>`     | Restore snapshot   |
| `delete <team>`             | Delete team        |
| `help`                      | Show team help     |

```
/team create code-review                              # Create team
/team add-member code-review reviewer-agent           # Add member
/team run code-review "Review src/*.rs"               # Run task
```

### `/agent [subcommand]`

Spawned agent management.

| Subcommand    | Description         |
| ------------- | ------------------- |
| `list`        | List spawned agents |
| `status <id>` | Agent status        |
| `stop <id>`   | Stop agent          |
| `logs <id>`   | Agent logs          |

### `/messaging [subcommand]`

Inter-agent messaging inspection.

| Subcommand | Description       |
| ---------- | ----------------- |
| `metrics`  | Metrics snapshot  |
| `dlq`      | Dead letter queue |
| `status`   | Mailbox status    |

### `/login`

Authenticate with the API.

### `/register`

Register a new account.

### `/logout`

Logout from the API.

### `/profile [subcommand]`

Manage user profile preferences.

| Subcommand     | Description                 |
| -------------- | --------------------------- |
| `show`         | Show current profile        |
| `edit <k> <v>` | Edit a preference           |
| `scenario`     | Show detected work scenario |
| `stats`        | Show profile usage stats    |
| `tools`        | Show blocked tool policy    |
| `experiments`  | Show enrolled experiments   |
| `reset`        | Reset preferences           |
| `help`         | Show profile help           |

### `/memory-setup`

Guided Memoria configuration wizard.

---

## 🔧 System Commands

### `/allow [mode]`

Set permission mode for tool execution.

| Mode                                     | Description                   |
| ---------------------------------------- | ----------------------------- |
| `auto`            | Auto-approve normal tool risk; some git/sensitive gates may still stop |
| `bypass` / `skip` | Skip approval prompts; catastrophic and policy hard-denies still apply |
| `plan`            | Read-only investigation mode  |
| `accept_edits`    | Auto-approve local file edits |
| `prompt`          | Prompt before each tool       |
| `deny`            | Deny all tool use             |
| `rules`           | Show current permission rules |

```
/allow auto      # Auto-approve normal tool risk
/allow bypass    # Skip approval prompts
/allow prompt    # Ask before each
/allow rules     # Show rules
```

### `/instructions [subcommand]`

Project instructions from `.astra/instructions.md`.

| Subcommand | Description                 |
| ---------- | --------------------------- |
| `show`     | Display loaded instructions |
| `reload`   | Reload from file            |
| `off`      | Disable for this session    |

### `/style [theme]`

Set output theme.

| Theme           | Description           |
| --------------- | --------------------- |
| `default`       | Standard theme        |
| `minimal`       | Minimal output        |
| `colorful`      | Enhanced colors       |
| `high-contrast` | Accessibility theme   |
| `list`          | List available themes |

### `/diagnostics`

Run diagnostic checks (binary, API, auth, environment).

### `/bug [copy|save]`

Generate a bug report.

```
/bug         # Display report
/bug copy    # Copy to clipboard
/bug save    # Save to file
```

---

## Keyboard Shortcuts

| Key       | Action                        |
| --------- | ----------------------------- |
| `Tab`     | Command/subcommand completion |
| `Ctrl+C`  | Cancel current input          |
| `Ctrl+D`  | Exit astra                    |
| `Up/Down` | History navigation            |
| `Ctrl+R`  | Reverse history search        |
| `Ctrl+L`  | Clear screen                  |

---

## Tips

1. **Command prefixes**: Most commands can be invoked with unique prefixes (e.g., `/he` for `/help`).

2. **Subcommand completion**: Press Tab after a command to see available subcommands.

3. **Environment variables**: Many behaviors can be controlled via env vars:
   - `ASTRA_FAST_STARTUP=1` — Skip animations
   - `ASTRA_STARTUP_TRACE=1` — Show startup timing
   - `MO_*` — Runtime configuration overrides

4. **Project instructions**: Create `.astra/instructions.md` for project-specific context.

5. **Skills**: Use `/skill new <name>` to create custom skills in `.astra/skills/`.

---

## REPL-to-TUI Migration Evaluation

This section tracks the evaluation of 23 legacy REPL `slash_*.rs` handler files
and their migration status towards TUI-native panels.

### Evaluation Criteria

| Fate          | Description                                              |
| ------------- | -------------------------------------------------------- |
| **KEEP**      | Core routing logic required by TUI fallback or dispatch  |
| **PORT**      | Logic to port into a native TUI panel (ViewStack portal) |
| **WRAP**      | Thin TUI selector/filter wrapping existing logic         |
| **DEPRECATE** | REPL-only command with no TUI value; superseded          |

### Handler Evaluation Table

| File                 | Lines | Fate          | TUI Equivalent                 | Notes                                                          |
| -------------------- | ----- | ------------- | ------------------------------ | -------------------------------------------------------------- |
| `slash_router.rs`    | 767   | **KEEP**      | `slash_dispatch.rs` (fallback) | Core dispatch orchestrator; called via `SlashResult::Fallback` |
| `slash_state.rs`     | 1524  | **KEEP**      | `SessionState`                 | State management used by many handlers                         |
| `slash_info.rs`      | 2712  | **PORT**      | `ContextPanel`                 | Session info → `/context` panel (Phase 1.3)                    |
| `slash_config.rs`    | 864   | **PORT**      | `ConfigPanel`                  | Config editor → `/config` panel (Phase 1.3)                    |
| `slash_skill.rs`     | 4170  | **PORT**      | `SkillBrowser`                 | Skill management → `/skill` selector (Phase 1.3)               |
| `slash_agent.rs`     | 2385  | **PORT**      | `AgentPanel`                   | Agent inspection → `/agent` panel (Phase 1.4)                  |
| `slash_team.rs`      | 2411  | **PORT**      | `TeamPanel`                    | Team management → `/team` panel (Phase 1.4)                    |
| `slash_mcp.rs`       | 1322  | **PORT**      | `McpPanel`                     | MCP server mgmt → `/mcp` panel (Phase 1.4)                     |
| `slash_memory.rs`    | 508   | **PORT**      | `MemoryPanel`                  | Memoria inspection → `/memory` panel (Phase 1.4)               |
| `slash_session.rs`   | 6198  | **PORT**      | `SessionPicker`                | Session mgmt → `/session` handler (Phase 1.3)                  |
| `slash_inspect.rs`   | 495   | **PORT**      | `InspectPanel`                 | Harness snapshot → `/inspect` handler (Phase 1.3)              |
| `slash_stats.rs`     | 670   | **WRAP**      | `StatsPanel`                   | Stats viewer → `/stats` selector (done)                        |
| `slash_plan.rs`      | 210   | **WRAP**      | `PlanMode`                     | Plan mode → `/plan` inline (done)                              |
| `slash_task.rs`      | 501   | **WRAP**      | `TaskBoard`                    | Task mgmt → `/task` panel (Phase 1.3)                          |
| `slash_profile.rs`   | 613   | **WRAP**      | Fallback                       | Profile mgmt → fallback to REPL handler                        |
| `slash_telemetry.rs` | 2196  | **DEPRECATE** | `StatusLine` + observability   | Replaced by TUI-native telemetry                               |
| `slash_health.rs`    | 225   | **DEPRECATE** | `StatusLine`                   | Health replaced by status bar indicators                       |
| `slash_debug.rs`     | 1414  | **DEPRECATE** | Dev tools                      | Debug tools; keep as fallback for dev builds                   |
| `slash_sync.rs`      | 28    | **DEPRECATE** | `StatusLine`                   | Sync status replaced by cloud sync indicator                   |
| `slash_tools.rs`     | 65    | **DEPRECATE** | N/A                            | Tools listing superseded by `/skill` browser                   |
| `slash_bug.rs`       | 174   | **DEPRECATE** | Chat                           | Bug report via chat composer                                   |
| `slash_messaging.rs` | 269   | **DEPRECATE** | `AgentPanel`                   | Messaging inspection folded into agent panel                   |
| `slash_account.rs`   | 258   | **DEPRECATE** | `LoginPanel`                   | Account mgmt via `/login` panel (done)                         |

### Migration Progress

- **Phase 1.1** (✅ complete): `TuiHandler` annotations on all commands
- **Phase 1.2** (✅ complete): Evaluation + catch-all routing in `slash_dispatch.rs`
- **Phase 1.3** (🔜 next): Wire Panel handlers to open native TUI panels
- **Phase 1.4**: Portalize remaining high-value commands

---

## See Also

- [CLI Commands Reference](./cli-commands.md) — `astra` and `astra admin` CLI commands
- [Skill Development Guide](../guides/skill-development.md) — Creating custom skills
- [MCP Integration](../guides/mcp-integration.md) — Adding MCP servers
