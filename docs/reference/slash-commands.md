# REPL Slash Commands Reference

Complete reference for slash commands available in the `astra interactive` REPL.

## Overview

The REPL supports 62 slash commands organized into 9 groups. Type `/help` to see all available commands, or `/help keys` for keyboard shortcuts.

## Command Groups

| Icon | Group | Description |
|------|-------|-------------|
| ⚡ | Core | Essential commands (help, model, session control) |
| 📂 | Workspace | Code search, diff, and review |
| 🔭 | Observability | Debugging, stats, and telemetry |
| 📋 | Session & Plan | Session management and structured planning |
| 🧠 | Memory & Tasks | Memoria integration and task management |
| 📦 | Skills | Skill management and marketplace |
| 🔌 | MCP | Model Context Protocol servers |
| 👥 | Team & Account | Multi-agent teams and authentication |
| 🔧 | System | Permissions, style, and diagnostics |

---

## ⚡ Core Commands

### `/help [keys]`
Show available commands and usage hints.

```
/help        # Show command palette
/help keys   # Show keyboard shortcuts
```

### `/model [name]`
List available models or switch to a different model.

```
/model                    # List models
/model claude-sonnet-4   # Switch to Sonnet 4
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
Exit the REPL.

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

| Subcommand | Description |
|------------|-------------|
| (none) | Show unstaged changes |
| `staged` | Staged vs HEAD |
| `stat` | Diff stat summary |
| `show <rev>` | Show specific revision |
| `patch` | Alias for unstaged |

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

| Subcommand | Description |
|------------|-------------|
| `history` | Journal-style conversation history |
| `errors` | Show session errors |
| `export` | Export to Markdown in cwd |
| `fork` | Fork session for experiments |
| `list` | List all sessions |
| `cleanup` | Clean stale sessions |
| `verify` | Verify session integrity |

```
/session list                    # List all
/session cleanup --days 7        # Clean old sessions
/session export                  # Export to Markdown
```

### `/plan [subcommand]`
Structured planning mode for complex tasks.

| Subcommand | Description |
|------------|-------------|
| (none) | Enter plan mode with a goal |
| `go` | Execute plan automatically |
| `step` | Execute one step at a time |
| `status` | Show plan progress |
| `show` | Display current plan |
| `pause` | Pause execution |
| `resume` | Resume execution |
| `exit` | Exit plan mode |
| `help` | Show plan commands |

```
/plan Build a REST API           # Start planning
/plan go                         # Auto-execute
/plan step                       # Step-by-step
/plan exit                       # Leave plan mode
```

### `/report [save]`
Show the last delivery report from plan execution.

```
/report        # Display report
/report save   # Save as JSON
```

---

## 🧠 Memory & Tasks Commands

### `/memory [subcommand]`
Memoria semantic memory operations.

| Subcommand | Description |
|------------|-------------|
| `list` | List memories |
| `search <q>` | Semantic search |
| `inspect <id>` | Inspect specific memory |

```
/memory list                     # List all
/memory search "auth pattern"    # Semantic search
/memory inspect mem_abc123       # Inspect by ID
```

### `/task [subcommand]`
Task management for async work.

| Subcommand | Description |
|------------|-------------|
| `list` | List tasks |
| `add <title>` | Create task |
| `done <id>` | Mark complete |
| `status <id>` | Check status |
| `run <prompt>` | Run task prompt |
| `result <id>` | Get task result |

```
/task list                       # List tasks
/task add "Review PR #123"       # Create task
/task done review-pr-123         # Complete task
```

---

## 🔭 Observability Commands

### `/explain`
Cycle through explanation modes: off → on (API) → verbose (+stderr).

### `/verbose`
Enable verbose streaming output.

### `/compact [mode]`
Summarize and trim conversation history.

| Mode | Description |
|------|-------------|
| (none) | Standard compaction |
| `quick` | Fast compaction without summary |
| `no-memoria` | Compact without Memoria |
| `summary-only` | Summarize without trimming |

### `/reflect [mode]`
Reflect on session (skill_failure, performance, etc.).

### `/turn [selector]`
Inspect specific turns.

```
/turn              # Latest turn
/turn list         # List all turns
/turn 5            # Turn by index
/turn -1           # Last turn
/turn id:abc123    # By turn ID
```

### `/debug`
Interactive session inspector for messages, tools, and context injections.

### `/stats [subcommand]`
Session analytics.

| Subcommand | Description |
|------------|-------------|
| (none) | Current session stats |
| `cost` | API cost estimate |
| `health` | Tool health dashboard |
| `history` | Aggregate stats across sessions |
| `learn` | Learning insights |
| `tools` | Tool performance metrics |

### `/lsp [status]`
LSP (Language Server Protocol) backend status.

### `/telemetry [subcommand]`
Session telemetry: turns, drift, decisions, profile.

### `/tuning [subcommand]`
Auto-tuning status and history.

| Subcommand | Description |
|------------|-------------|
| `status` | Current tuning state |
| `history` | Tuning history |
| `config` | Tuning configuration |
| `reset` | Reset tuning state |

### `/sync [subcommand]`
Cloud sync status and operations.

| Subcommand | Description |
|------------|-------------|
| (none) | Show sync status |
| `log` | Recent sync events |
| `push` | Force push to cloud |
| `pull` | Pull from cloud |

### `/context`
Show context window and token budget summary.

### `/rewind <turn>`
Rewind conversation to an earlier turn.

### `/version`
Display version information.

---

## 📦 Skills Commands

### `/skill [subcommand]`
Skill management and marketplace.

| Subcommand | Description |
|------------|-------------|
| `list [filter]` | List skills |
| `info <name>` | Skill details |
| `search <q>` | Keyword search |
| `browse` | Browse marketplace |
| `install <name>` | Install from marketplace |
| `new <name>` | Create new skill |
| `test <name>` | Run skill test |
| `dev <name>` | Enter dev mode |
| `health` | Catalog health |
| `surfacing` | Agent catalog surfacing |
| `pin <name>` | Pin skill to always load |
| `create` | Generate skill from session |
| `system` | System skill helpers |
| `stats` | Learning summary |

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

| Subcommand | Description |
|------------|-------------|
| `status` | Connection status table |
| `servers` | Server details and tools |
| `prompts` | List available prompts |
| `resources` | List available resources |
| `add <name> <cmd>` | Add MCP server |
| `remove <name>` | Remove server |
| `ping [server]` | Ping server(s) |
| `prompt <server>:<name>` | Invoke prompt |
| `resource <server>:<uri>` | Read resource |
| `subscribe <server>:<uri>` | Subscribe to resource |
| `unsubscribe <server>:<uri>` | Unsubscribe |
| `log-level <server> <level>` | Set log level |
| `complete <ref> <arg>` | Get completions |

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

| Subcommand | Description |
|------------|-------------|
| `list` | List teams |
| `info <name>` | Team information |
| `create <name>` | Create team |
| `add-member <team> <agent>` | Add member |
| `context <team> <ctx>` | Set shared context |
| `run <team> <task>` | Run task with team |
| `history <team>` | Execution history |
| `snapshot <team>` | Save snapshot |
| `restore <team> <snap>` | Restore snapshot |
| `delete <team>` | Delete team |
| `help` | Show team help |

```
/team create code-review                              # Create team
/team add-member code-review reviewer-agent           # Add member
/team run code-review "Review src/*.rs"               # Run task
```

### `/agent [subcommand]`
Spawned agent management.

| Subcommand | Description |
|------------|-------------|
| `list` | List spawned agents |
| `status <id>` | Agent status |
| `stop <id>` | Stop agent |
| `logs <id>` | Agent logs |

### `/messaging [subcommand]`
Inter-agent messaging inspection.

| Subcommand | Description |
|------------|-------------|
| `metrics` | Metrics snapshot |
| `dlq` | Dead letter queue |
| `status` | Mailbox status |

### `/login`
Authenticate with the API.

### `/register`
Register a new account.

### `/logout`
Logout from the API.

### `/memory-setup`
Guided Memoria configuration wizard.

---

## 🔧 System Commands

### `/allow [mode]`
Set permission mode for tool execution.

| Mode | Description |
|------|-------------|
| `auto` | Auto-approve all tool use |
| `all` | Alias for auto |
| `prompt` | Prompt before each tool |
| `deny` | Deny all tool use |
| `rules` | Show current permission rules |

```
/allow auto      # Trust all tools
/allow prompt    # Ask before each
/allow rules     # Show rules
```

### `/yolo`
Alias for `/allow auto` (auto-approve all tools).

### `/instructions [subcommand]`
Project instructions from `.astra/instructions.md`.

| Subcommand | Description |
|------------|-------------|
| `show` | Display loaded instructions |
| `reload` | Reload from file |
| `off` | Disable for this session |

### `/style [theme]`
Set output theme.

| Theme | Description |
|-------|-------------|
| `default` | Standard theme |
| `minimal` | Minimal output |
| `colorful` | Enhanced colors |
| `high-contrast` | Accessibility theme |
| `list` | List available themes |

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

| Key | Action |
|-----|--------|
| `Tab` | Command/subcommand completion |
| `Ctrl+C` | Cancel current input |
| `Ctrl+D` | Exit REPL |
| `Up/Down` | History navigation |
| `Ctrl+R` | Reverse history search |
| `Ctrl+L` | Clear screen |

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

## See Also

- [CLI Commands Reference](./cli-commands.md) — `astra` and `astra-admin` CLI commands
- [Skill Development Guide](../guides/skill-development.md) — Creating custom skills
- [MCP Integration](../guides/mcp-integration.md) — Adding MCP servers
