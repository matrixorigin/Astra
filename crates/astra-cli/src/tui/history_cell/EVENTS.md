# Stream Event → HistoryCell Mapping

This table maps every user-visible event to its HistoryCell type,
helping new contributors understand the full "business event" surface.

| Stream/App Event | Cell Type | Module | Visual |
|---|---|---|---|
| User submits text | `UserCell` | `user.rs` | `› <text>` |
| Model streams answer tokens | `AssistantCell` | `assistant.rs` | Markdown body |
| Model produces reasoning/thinking | `ReasoningCell` | `reasoning.rs` | Dim italic block |
| Tool starts (top-level) | `ToolCell` | `tool.rs` | Blue-framed active cell |
| Tool completes | `ToolCell` (finalized) | `tool.rs` | `✓ <name> (dur)` |
| Tool starts (child of task) | `TaskCell` child entry | `task.rs` | `├ • <name>` |
| System info/response/error | `SystemCell` | `system.rs` | `⎿ <msg>` / `⚠ <err>` |
| Task tool started (parent) | `TaskCell` | `task.rs` | `▶ Task <desc>` |
| Task completed/failed | `TaskCell` (finalized) | `task.rs` | `▶ Task done/failed` |
| Turn complete | `TurnSummaryCell` | `turn_summary.rs` | `── turn N ──` |

## Collapsed vs Inline

`TaskCell` with > 3 children auto-collapses after completion:
- Collapsed: `└ 12 tools · 10 succeeded, 2 failed`
- Expand via `TaskDetailView` (push to view_stack)

## Non-Cell Events (handled by indicators, not scrollback)

| Event | Handler | Visual |
|---|---|---|
| `WaitingForModel` | `StatusIndicator` | Spinner in status line |
| `ModelResponding` | `StatusIndicator` | "Thinking" label |
| `ThinkingStarted/Chunk/Stopped` | `StatusIndicator` + `ReasoningCell` | Dim preview |
| `ToolOutput { lines, bytes }` | `ToolCell::set_progress` | Counter update |
| `StatusLine` | Footer | One-line override |
