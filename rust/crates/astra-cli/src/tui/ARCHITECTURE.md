# Astra TUI Architecture Guide

## Visual Layout

```
┌─────────────────────────────────────────────────────────────────┐
│                    Terminal Scrollback                           │
│  (completed turns: user messages, assistant responses,          │
│   tool cells, turn summaries — managed by insert_history.rs)    │
│                                                                 │
│  › user message                                                 │
│                                                                 │
│  • Assistant response line 1                                    │  ← AgentMessageCell
│    continuation line 2                                          │    (mini-cells flushed
│    continuation line 3                                          │     incrementally during
│                                                                 │     streaming)
│  • Ran bash (52ms)                                              │  ← ToolChatCell
│    │ $ echo hello                                               │
│    └ 1 line captured                                            │
│                                                                 │
│  • Next response...                                             │
│                                                                 │
│  ─ tokens:7.2k (↑7.1k ↓85) │ 2.3s │ ttft:450ms │ cache:80% ─  │  ← Turn Summary
│                                                                 │
├─────────────────────────── Viewport ────────────────────────────┤
│                                                                 │
│  • Working (2.3s • esc to interrupt)                            │  ← Active Cell
│    (shimmer animation during thinking/waiting)                  │    (only shows during
│                                                                 │     thinking, NOT during
│────────────────────────────────────────────────────────────────│     text streaming)
│  › Ask astra to do anything                                     │  ← Composer
│                                                                 │     (or Overlay Panel)
│  / commands · $ skills · Ctrl+O transcript     ~/dir · 7k↑ 85↓ │  ← Footer
│                                                                 │
└─────────────────────────────────────────────────────────────────┘
```

## Component Naming Guide

### Screen Regions

| Name | Description | Location |
|------|-------------|----------|
| **Scrollback** | Terminal native scrollback above the viewport. Completed messages, tool results, turn summaries. User can scroll up with terminal scrollwheel. | Top of screen, grows upward |
| **Viewport** | Fixed-height area at screen bottom managed by ratatui. Contains Active Cell + Separator + Bottom Pane. | Bottom N rows of screen |
| **Active Cell** | Transient display area in viewport for thinking shimmer. Empty during text streaming (text goes to scrollback). | Top of viewport |
| **Separator** | Thin `────` dim line between scrollback and composer. Visual boundary. | Between active cell and bottom pane |
| **Bottom Pane** | Composer + Footer (normal), or Overlay Panel (when a view is active). | Bottom of viewport |

### Bottom Pane Components

| Name | Description | When Visible |
|------|-------------|-------------|
| **Composer** | Input line with `› ` prefix. User types here. | Always (unless overlay active) |
| **Footer** | Status bar: shortcuts hint (left) + model · dir · tokens (right) | When no popup/overlay active |
| **Slash Popup** | Command list below composer, triggered by `/`. Filters as user types. | When composer text starts with `/` |
| **Skill Popup** | Skill mention list below composer, triggered by `$`. | When composer text starts with `$` |
| **Overlay Panel** | Full bottom-pane replacement. Used by views like HelpView, ListSelectionView, etc. Esc closes. | When a view is pushed to view_stack |

### Overlay Panel Types (BottomPaneView implementations)

| Name | Trigger | Description |
|------|---------|-------------|
| **ListSelectionView** | `/model`, `/skill`, `/stats` menu | Numbered list with `›` selection, Up/Down, Enter/Esc |
| **HelpView** | `/help` | Tabbed command browser. ←/→ switch groups, ↑/↓ browse, Enter inserts command |
| **InfoView** | `/stats` sub-views, `/whoami`, `/instructions show` | Scrollable key-value or text display. `reopen` field for Esc-back to parent menu |
| **HistoryView** | `/history` | Conversation history with real-time search bar. Type to filter, ↑/↓ scroll |
| **TranscriptView** | `Ctrl+O` | Full conversation record including hidden thinking content. Scroll with ↑/↓/PgUp/PgDn |
| **ApprovalOverlay** | Tool approval request (auto) | Y/N approval dialog for tool execution |

### Chat Cells (content units in scrollback)

| Name | File | Description |
|------|------|-------------|
| **UserChatCell** | `user_cell.rs` | `› ` bold prefix + user message text |
| **AgentMessageCell** | `agent_message_cell.rs` | Mini streaming cell. `• ` first line, `  ` continuation. Flushed to scrollback per commit tick (1-5 lines). Adaptive-wrapped to terminal width. |
| **AssistantChatCell** | `assistant_cell.rs` | Used ONLY for thinking/shimmer display in viewport. NOT used for streamed text. Has thinking_chunks for transcript. |
| **ToolChatCell** | `tool_cell.rs` | `• Ran tool (Nms)` header + `│` command + `└` output. Diff lines get green/red coloring. Full output in transcript. |
| **SystemChatCell** | `system_cell.rs` | Dimmed info/warning/error messages |

### Turn Summary

After each turn completes, a dim separator line is written to scrollback:
```
  ─ model:name │ tokens:7.2k (↑7.1k ↓85) │ $0.0012 │ 2.3s │ ttft:450ms │ 2 tools │ cache:80% ─
```

## Data Flow: Streaming Token → Scrollback

```
SSE Stream
    │
    ▼
StreamEvent::Token(text)
    │
    ▼ (stream_bridge.rs)
TuiAppEvent::Token(text)
    │
    ▼ (mod.rs handle_app_event)
StreamController.push_delta(text)
    │                                              ┌─────────────────────┐
    ├── newline crossed? ──yes──►  on_commit_tick_batch(5)               │
    │                              │                                     │
    │                              ▼                                     │
    │                       emit() → AgentMessageCell                    │
    │                              │                                     │
    │                              ▼                                     │
    │                       flush_mini_cell()                            │
    │                              │                                     │
    │                              ├──► guard.queue_history_lines()      │
    │                              │         │                           │
    │                              │         ▼ (next draw frame)         │
    │                              │    insert_history_lines()           │
    │                              │    (ANSI: DECSTBM + RI + write)    │
    │                              │         │                           │
    │                              │         ▼                           │
    │                              │    Terminal Scrollback              │
    │                              │                                     │
    │                              └──► transcript.extend()              │
    │                                                                    │
    └── no newline ──► buffered in collector (wait for \n)               │
                                                                         │
                                                                         │
drain_tick (every 80ms)                                                  │
    │                                                                    │
    ▼                                                                    │
StreamController.on_commit_tick()                                        │
    │                                                                    │
    ▼                                                                    │
Same path: emit() → flush_mini_cell() → scrollback ─────────────────────┘


Turn End:
    │
    ▼
StreamController.finalize()
    │
    ▼
Remaining lines → final AgentMessageCell → flush_mini_cell()
    │
    ▼
Trailing blank lines + Turn Summary line → scrollback
```

## Data Flow: Slash Command

```
User types "/mo" + Tab
    │
    ▼
Composer text = "/model "
    │
    ▼ (bottom_pane handle_key)
Slash Popup syncs (filters commands matching "mo")
    │
    ▼
User presses Enter (popup visible)
    │
    ▼
BottomPaneAction::SubmitInput("/model")
    │
    ▼ (mod.rs)
slash_dispatch::dispatch("/model", ctx)
    │
    ├── TUI-native (● marker)? ──yes──► Inline handling
    │       /model → ListSelectionView pushed
    │       /help  → HelpView pushed
    │       /stats → menu → sub-view
    │       /copy  → clipboard copy
    │       /exit  → exit
    │
    └── Fallback ──► guard.with_restored(|| slash_router::handle_slash_command())
            (temporarily exits TUI, runs in line mode, restores TUI)
```

## Key Files

| File | Lines | Role |
|------|-------|------|
| `mod.rs` | 793 | Main event loop, draw cycle, streaming orchestration |
| `custom_terminal.rs` | 763 | Dual-buffer diff terminal (ported from Codex, MIT) |
| `slash_dispatch.rs` | 621 | All slash command inline handling |
| `wrapping.rs` | 1407 | URL-aware span-preserving word wrap (ported from Codex, MIT) |
| `bottom_pane/mod.rs` | 304 | Composer + Footer + Popup + View stack orchestration |
| `bottom_pane/textarea.rs` | 550 | Multi-line text editor (Emacs keybindings, word-jump, kill buffer) |
| `markdown_render.rs` | 324 | pulldown-cmark → ratatui Lines with syntect code highlighting |
| `insert_history.rs` | 245 | ANSI escape sequences for writing to terminal scrollback |
| `terminal.rs` | 238 | TerminalGuard lifecycle, draw sequence, with_restored |
| `render/highlight.rs` | 231 | Syntect + two_face syntax highlighting (250+ languages, 32+ themes) |
| `streaming/controller.rs` | 162 | Newline-gated streaming, mini-cell emission |
| `diff_render.rs` | 155 | Diff line rendering with line numbers, gutter signs, theme-aware colors |

## Keyboard Shortcuts

| Key | Context | Action |
|-----|---------|--------|
| `Enter` | Composer | Submit text / dispatch slash command |
| `Shift+Enter` | Composer | Insert newline (multi-line input) |
| `Ctrl+C` | Turn active | Cancel/interrupt turn |
| `Ctrl+C` | Composer non-empty | Clear draft |
| `Ctrl+C` | Idle | Quit |
| `Ctrl+D` | Composer empty | Quit |
| `Ctrl+L` | Any | Force full redraw |
| `Ctrl+O` | Idle | Open transcript view |
| `Esc` | Overlay/Popup | Close and return |
| `/` | Composer | Slash command popup |
| `$` | Composer | Skill mention popup |
| `Tab` | Slash popup | Autocomplete selected command |
| `↑/↓` | Popup/View | Navigate items |
| `←/→` | HelpView | Switch command group tab |
| `Ctrl+A/E` | Composer | Line start/end |
| `Ctrl+K/Y` | Composer | Kill line / yank |
| `Ctrl+W` | Composer | Delete backward word |
| `Ctrl+←/→` | Composer | Word jump |
