# Astra TUI Architecture Guide

## Visual Layout

```
┌─────────────────────────────────────────────────────────────────┐
│                    Terminal Scrollback                           │
│  (committed HistoryCells flushed by ChatWidget; user can scroll  │
│   up with the terminal's native scrollwheel)                     │
│                                                                 │
│  › user message                                                 │  ← UserCell
│                                                                 │
│  ┃ assistant response, streamed word by word, markdown-rendered │  ← AssistantCell
│  ┃ with the accent gutter ("┃ ") on every wrapped row            │
│                                                                 │
│  • Ran bash (52ms)                                              │  ← ToolCell
│    │ $ echo hello                                               │
│    └ 1 line captured                                            │
│                                                                 │
│  ─ ⏱ 2.3s │ ⚡ 7.2k ↑7.1k ↓85 │ 🛠 2 │ Σ 12.5k · $0.014 ─        │  ← TurnSummaryCell
│                                                                 │
├─────────────────────────── Viewport ────────────────────────────┤
│                                                                 │
│  ✶ Thinking … (2.3s · ↓ 340 tok)                                │  ← StatusIndicator
│                                                                 │     or the active
│────────────────────────────────────────────────────────────────│     HistoryCell
│  › Ask astra to do anything                                     │  ← Composer
│  / commands · $ skills · Ctrl+O transcript     ~/dir · 7k↑ 85↓ │  ← Footer
│                                                                 │
└─────────────────────────────────────────────────────────────────┘
```

## Core model

Everything on screen is a **HistoryCell**. One trait, one source of truth
(`ChatWidget.history: Vec<Arc<dyn HistoryCell>>`), one rendering pipeline.

- **Committed cells** live in `history` and are immutable. They flush to
  terminal scrollback exactly once (via `drain_new_committed`) and persist
  to `~/.astra/transcripts/<session_id>.jsonl` via `HistoryCell::to_persist`.
- **One live cell** at a time lives in `ChatWidget.active_cell`. It's
  rendered above the composer every frame by `display_lines(width)` — no
  cache, so terminal resize / theme changes are handled for free.
- **Events** arrive as `AppEvent` (translated from the on-the-wire
  `TuiAppEvent` by `chat_widget::bridge::translate`). `ChatWidget::handle_event`
  is a single `match` that mutates `history` / `active_cell`.

### Screen regions

| Name | Description | Location |
|------|-------------|----------|
| **Scrollback** | Terminal native scrollback above the viewport. Committed HistoryCells flushed here. | Top of screen, grows upward |
| **Viewport** | Fixed-height region at screen bottom managed by ratatui. Active cell + separator + bottom pane. | Bottom N rows |
| **Active-cell area** | Live `HistoryCell` (streaming assistant, running tool, reasoning) or `StatusIndicator` fallback. | Top of viewport |
| **Separator** | Thin `────` dim line between active cell and composer. | Between active cell and bottom pane |
| **Bottom Pane** | Composer + Footer, or an overlay view (HelpView, TranscriptView, …). | Bottom of viewport |

### Bottom-pane components

| Name | Description | When visible |
|------|-------------|--------------|
| **Composer** | Input line with `› ` prefix. Emacs keybindings, multi-line with Shift+Enter. | Always (unless an overlay is active) |
| **Footer** | Shortcuts hint (left) + model · dir · tokens · cost (right) | When no popup/overlay is active |
| **Slash Popup** | Command list under composer, filters as user types `/…` | Composer text starts with `/` |
| **Skill Popup** | Skill mention list, triggered by `$` | Composer text starts with `$` |
| **Overlay Panel** | Full bottom-pane replacement (HelpView, ListSelectionView, TranscriptView, …). Esc closes. | When a view is pushed onto view_stack |
| **Approval Cell** | `⏸ bash wants to run …` with focused button, rendered above composer. | When an approval is pending. Now a `HistoryCell` (not scrollback-committed; `to_persist` is `None`). |

### Overlay panels (BottomPaneView implementations)

| Name | Trigger | Description |
|------|---------|-------------|
| **ListSelectionView** | `/model`, `/skill`, `/stats` menu | Numbered list with `›` selection |
| **HelpView** | `/help` | Tabbed command browser |
| **InfoView** | `/stats` detail, `/whoami`, `/instructions show` | Scrollable key-value display |
| **HistoryView** | `/history` | Conversation history with search bar |
| **TranscriptView** | `Ctrl+O` | Scrollable committed-history overlay; scales to 80% of terminal height |
| **SessionPickerView** | `/resume` (no args) | Two-pane recent sessions picker |
| **LoginView / RegisterView** | `/login`, `/register` | Inline auth form (no drop to bare terminal) |

### Cell types

All cells live in `history_cell/` and implement the `HistoryCell` trait
(`display_lines` / `as_any*` / `to_persist` / `finalize` / `is_live`).

| Cell | File | Role |
|------|------|------|
| **UserCell** | `user.rs` | `›` accent-bold prefix + user message text |
| **AssistantCell** | `assistant.rs` | Streaming markdown with `┃ ` accent gutter; blinking cursor while live |
| **ReasoningCell** | `reasoning.rs` | `💭 Thinking` header + compact body; committed on `ReasoningDone` or first answer delta |
| **ToolCell** | `tool.rs` | `• Ran tool (Nms)` header + `│` args + `└` output; diff-aware coloring |
| **SystemCell** | `system.rs` | info / warning / error; error text is humanized (strips `<tool_use_error>` wrappers) |
| **TurnSummaryCell** | `turn_summary.rs` | `─ ⏱ N.Ns │ ⚡ …in ↓…out │ 🛠 N │ Σ N · $C ─` end-of-turn band |
| **ApprovalCell** | `approval.rs` | Inline approval prompt with focused button; not persisted |

## Data flow: streaming token → scrollback

```
SSE Stream
    │
    ▼ (stream_bridge.rs)
TuiAppEvent::Token(text)
    │
    ▼ (mod.rs outer select! → chat_widget::translate)
AppEvent::AnswerDelta(text)
    │
    ▼ ChatWidget::handle_event
on_answer_delta(&text)
    │   ├── active_cell is None or Reasoning?
    │   │       └── commit prior cell, start new AssistantCell (live)
    │   └── push_delta into active AssistantCell.source
    │
    ▼ (next frame)
do_draw reads active_cell.display_lines(width)
    │   — re-renders markdown from source each frame (no cache)
    │   — blinking ▎ cursor appended while live
    │
    ▼ (turn end: TurnComplete event)
ChatWidget::handle_event(AppEvent::TurnComplete(stats))
    │   ├── commit_active() → finalize + persist + move to history
    │   └── commit_cell(TurnSummaryCell { stats })
    │
    ▼ (outer loop)
flush_chat_widget(guard, &mut chat_widget, width)
    │   — drain_new_committed() returns new cells since last flush
    │   — each cell's display_lines pushed to scrollback with a trailing blank
```

## Data flow: slash command

```
User types "/mo" + Tab
    │
    ▼ Composer auto-completes to "/model "
    ▼ Slash Popup filters to matching commands
    ▼ User presses Enter
    │
    ▼
BottomPaneAction::SubmitInput("/model")
    │
    ▼ (mod.rs)
slash_dispatch::dispatch("/model", ctx)
    │
    ├── TUI-native? ──► Handle inline (push view, emit SystemCell::info, …)
    │       /model → ListSelectionView
    │       /help  → HelpView
    │       /stats → menu → sub-view
    │       /copy  → clipboard
    │       /exit  → exit
    │
    └── SlashResult::Fallback ──► guard.with_restored(|| slash_router::handle_slash_command())
            │  (briefly leaves TUI, runs in line mode, restores)
            ▼
        If state.session_id changed (resume/new-session),
        rebuild ChatWidget via `replay_session_into_widget`:
          — load JSONL transcript, rebuild cells, paint banner,
            then mark_all_flushed so they don't re-flush.
```

## Resume flow (startup or `/resume`)

1. `handle_resume_command` → `restore_session_into_state` → `apply_restored_session`
   repopulates `state.history` / `state.session_id` / `state.runtime_continuity`
   / `state.csl_manager` from the session's CSL or journal on disk.
2. In the TUI, when `state.session_id` changes, `replay_session_into_widget`
   creates a fresh `ChatWidget` seeded from
   `~/.astra/transcripts/<sid>.jsonl` via `chat_widget::load_resume`.
3. The widget's committed cells are painted to scrollback exactly once
   with a `Resumed session <short-sid> — N cells restored` banner, then
   `mark_all_flushed()` advances the watermark so future ticks only
   surface new activity.

## Key files

| File | Role |
|------|------|
| `mod.rs` | Outer event loop, draw cycle, resume replay, turn orchestration |
| `chat_widget/mod.rs` | `ChatWidget` + `AppEvent` + `handle_event` router |
| `chat_widget/bridge.rs` | `TuiAppEvent → AppEvent` translator |
| `chat_widget/resume.rs` | Load JSONL → ChatWidget for resume |
| `chat_widget/turn_driver.rs` | E2E test harness (full turn → scrollback snapshot) |
| `history_cell/*.rs` | The 7 cell types + the `HistoryCell` trait |
| `turn_event.rs` | Discriminated-enum wire format for persistence |
| `transcript_jsonl.rs` | Append / load JSONL at `~/.astra/transcripts/<sid>.jsonl` |
| `status_indicator.rs` | Viewport status line (Thinking ✶ / Tool / WaitingModel / Idle) |
| `bottom_pane/` | Composer, popups, overlay views, approval queue |
| `slash_dispatch.rs` | TUI-native slash command handling (with Fallback for complex async) |
| `terminal.rs` | `TerminalGuard` lifecycle, inline viewport, `with_restored` |
| `custom_terminal.rs` | Dual-buffer diff terminal (inline-viewport with scroll) |
| `wrapping.rs` | URL-aware span-preserving word wrap |
| `markdown_render.rs` | pulldown-cmark → ratatui Lines with syntect code highlighting |
| `insert_history.rs` | ANSI escape sequences for writing to terminal scrollback |

## Keyboard shortcuts

| Key | Context | Action |
|-----|---------|--------|
| `Enter` | Composer | Submit text / dispatch slash command |
| `Shift+Enter` | Composer | Insert newline (multi-line input) |
| `Ctrl+C` | Turn active | Cancel/interrupt turn |
| `Ctrl+C` | Composer non-empty | Clear draft |
| `Ctrl+C` | Idle | Quit |
| `Ctrl+D` | Composer empty | Quit |
| `Ctrl+L` | Any | Force full redraw |
| `Ctrl+O` | Idle | Open transcript overlay (scales to terminal height) |
| `Ctrl+R` | Idle, composer empty | Pull last user message back into composer for editing / retry |
| `Ctrl+U` | Composer | Kill to start of line |
| `Esc` | Overlay/Popup | Close and return |
| `/` | Composer | Slash command popup |
| `$` | Composer | Skill mention popup |
| `Tab` | Slash popup | Autocomplete selected command |
| `↑/↓` | Popup/View | Navigate items |
| `←/→` | HelpView / Approval | Switch tab / focus button |
| `Ctrl+A/E` | Composer | Line start/end |
| `Ctrl+K/Y` | Composer | Kill line / yank |
| `Ctrl+W` | Composer | Delete backward word |
| `Ctrl+←/→` | Composer | Word jump |

## Design doc

The full rationale and phase-by-phase migration plan live in
`docs/design/tui-refactor.md`.
