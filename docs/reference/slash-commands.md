# TUI Slash Commands Reference

This is the complete catalog of the 28 slash-command roots with a native TUI
interaction. The canonical metadata lives in
[`command_registry.rs`](../../crates/astra-cli/src/cli/command_registry.rs).

## Discover commands

- Type `/` to open the complete TUI command list. Featured actions sort first;
  every other native command remains in the list and is reachable by scrolling
  or typing a filter.
- Run `/help` for the Featured tab and the complete catalog arranged by task.
  Select an entry to insert it into the composer.
- Run `/help keys` for the current workbench keyboard shortcuts.

Commands that only work in line mode are intentionally omitted from TUI
completion. Typing one in the workbench shows an unavailable message; Astra
never switches to a second terminal interface to complete it.

## Common

| Command | What it does |
| --- | --- |
| `/help` | Browse the full command catalog. |
| `/model` | Choose, inspect, or switch the active model. |
| `/copy` | Copy the latest assistant response. |
| `/exit` | Exit Astra. |

`/model` opens the picker with no arguments. Use `/model info` to inspect the
current model, `/model clear` to clear the selection, or `/model <name>` to
switch directly. `/model list` remains accepted as an alias for the picker,
but the bare command is the suggested form.

## Sessions

| Command | What it does |
| --- | --- |
| `/clear` | Start a fresh conversation. |
| `/history` | Open the complete conversation transcript. |
| `/resume [session_id]` | Choose a recent session or resume a specific session. |
| `/timeline` | Browse this session's turn-by-turn journal. |
| `/session` | View this session's overview and related actions. |

`/session analyze [id]` opens a concise session summary; `/session history [id]`
opens that transcript; and `/session export [id]` writes it as Markdown in the
current working directory. `/session list` remains accepted as an alias for
`/resume`. `/session fork` is a line-mode command and is not available in the
TUI.

If a new session targets a checkout already owned by another session, admission
stops before model or tool work. The TUI keeps the new session attached and
returns the draft to its composer; use `/resume` and choose a session explicitly
to continue existing work, or switch to another worktree for the new session.
Astra never resumes or takes over a session implicitly.

## Work

| Command | What it does |
| --- | --- |
| `/worktrees` | Browse repository worktrees and their session counts. |
| `/stop` | Stop the active run. |
| `/plan` | Enter or exit plan mode, then describe the plan in the composer. |
| `/work` | Open the Work board or start durable work. |
| `/tasks` | Open the live background task panel. |
| `/agent` | Open the agent monitor for active and recent runs. |

`/work` opens the canonical durable Work board. Use `/work start <goal>` to track the
current conversation as durable Work. If this TUI has not sent a message yet,
the command creates and binds its durable Session automatically; no throwaway
message or `/resume` is required. `/work status` remains accepted as an alias
for the board. If you press Enter with a normal message while that first Work
action is still starting, the message is shown in the queue and sent after the
same Session is attached. A deliberate Session switch leaves the message in
the composer for review instead of sending it to the wrong conversation. `/tasks`
opens the live shell and local-agent task panel used by Shift+Down/Ctrl+B; it
reuses the same session-bound registry and controls.

Selecting an agent in `/agent` opens its conversation and work record, where
you can inspect, guide, pause, resume, or stop the run. `/agent list` remains
accepted as an alias for the monitor.

## Inspect

| Command | What it does |
| --- | --- |
| `/explain [on\|verbose\|off] [--format html\|markdown\|text]` | Show measured execution facts at the selected detail and choose the derived local report format. A bare `/explain` is the idempotent `on` form; format-only commands keep the current mode. |
| `/reflect` | Review session evidence with a read-only reflection. |
| `/inspect` | Open the current runtime inspector. |
| `/stats` | Browse session, tool, cost, health, and learning stats. |
| `/context` | Inspect the context window. |
| `/info` | View version, session, model, permissions, and skill status. |

`/reflect [topic[/facet] [depth] [question]]` narrows the reflection; `/reflect
diff` compares evidence. `/stats` opens a selector, or you can open a focused
view with `/stats cost`, `/stats health`, `/stats history`, `/stats learn`, or
`/stats tools`. `/context dump [path]` writes a JSON snapshot to a file.

`/explain` changes presentation only; it never changes the recorded execution
facts. `on` keeps subsequent execution concise, `verbose` adds context,
dependency, and coverage details, and `off` suppresses the projection for
subsequent execution while retaining durable evidence and already recorded
history. The local report defaults to HTML; Markdown and text remain explicit
derived representations. Invalid arguments leave both mode and format
unchanged.

## Tools

| Command | What it does |
| --- | --- |
| `/memory` | Browse and search saved memories. |
| `/skill` | Browse available skills and activate one. |
| `/mcp` | Explore connected MCP servers, tools, prompts, and resources. |

`/memory` lists saved memories. Other TUI actions are `/memory search <query>`,
`/memory stats`, `/memory health`, and `/memory session`. `/memory list` and
`/memory ls` remain accepted aliases for the list.

`/skill` opens the skill browser; `$` opens the same browser from the composer.
`/skill browse` and `/skill list` remain accepted aliases. Marketplace
management actions such as install and publish do not have a TUI flow.

`/mcp` opens usage guidance. Its TUI discovery actions are `list`, `servers`,
`tools`, `inspect`, `prompts`, `resources`, `read`, `ping`, and `history`.
`/mcp status` remains accepted as an alias for `/mcp list`. The TUI MCP surface is
read-only; server configuration changes are not available here.

## Settings

| Command | What it does |
| --- | --- |
| `/config` | Edit runtime configuration. |
| `/allow` | Choose a permission mode and manage workspace trust. |
| `/instructions` | View, reload, or disable project instructions. |
| `/login` | Discover the connected server's login method: UC/Memoria browser sign-in or a self-hosted password form. |
| `/register` | Open the discovered provider's website for registration, or the self-hosted registration form. |

`/config edit` remains accepted as an alias for `/config`. `/allow` opens a mode
picker with no arguments. Its modes include `auto`, `bypass`, `read_only`,
`accept_edits`, `prompt`, and `deny`; additional actions show rules, manage
workspace trust, or inspect the permission trace. The picker asks for confirmation
before enabling `bypass`. `Shift+Tab` explicitly cycles
`Ask → Edits → Read-only → Auto → Bypass → Ask`; `Deny` remains an explicit
`/allow` choice. Shift+Tab also works while an inline approval is waiting;
Tab navigates approval entries, while open modal pickers retain their own navigation.
During execution the selected mode appears as `next: …` until
the server applies it before the next model round. It does not wait for another
user message. Already executing tools retain their captured policy. The current
mode chip changes only after execution acknowledges the change. If the response
ends first, the selection becomes the next response's mode.

`/instructions` opens the project-instructions actions. The accepted forms are
`/instructions show`, `/instructions reload`, and `/instructions off`.

## Accepted aliases and availability

The command picker shows one entry per action. Accepted aliases remain
available when typed, so existing workflows continue to work without adding
redundant completion rows:

| Suggested form | Also accepted |
| --- | --- |
| `/model` | `/model list` |
| `/work` | `/work status` |
| `/memory` | `/memory list`, `/memory ls` |
| `/agent` | `/agent list` |
| `/config` | `/config edit` |
| `/mcp list` | `/mcp status` |
| `/skill` | `/skill browse`, `/skill list` |
| `/resume` | `/session list` |

Line-mode-only commands such as `/grep`, `/diff`, and `/review` are not TUI
commands. Use `/` to see every root command that the workbench can complete.
