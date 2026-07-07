# TUI refactor — design doc

**Status:** draft, awaiting review.
**Author:** Claude (via xupeng).
**Supersedes:** nothing formal; absorbs the accumulated patch-series
`befe51c8b..d1cfb0f3a` (orbiter, thinking window, transcript persist).
**Scope:** `crates/astra-cli/src/tui/`.

---

## 1. Why we're rewriting, not patching

The last ten commits fixed visible symptoms — thinking preview, table
overflow, orbiter noise, transcript persistence — but every fix touched
a different module and re-introduced the next bug. The feedback was
blunt and correct: *“是系统性的修复或者重构,还是修修补补?”*

Root causes, in order of damage:

1. **Three unjoined stores for the same data.**
   - `guard.queue_history_lines(...)` writes ratatui `Line`s straight
     to terminal scrollback (volatile, can't be reread).
   - `transcript: Vec<Line>` collects a parallel copy for Ctrl+O.
   - `transcript_store::append(sid, lines)` persists to disk.
   Every time a new cell type or flow lands, we have to remember to
   write to all three. The *thinking preview* bug and the `/resume`
   replay bug both come straight from this.

2. **Four widgets implement the same conceptual "assistant cell".**
   `AssistantChatCell`, `AgentMessageCell`, `StreamController`,
   `orbiter_line`. Each owns a slice of the turn lifecycle. A property
   like "is the turn still streaming?" has no authoritative home — we
   read it from whichever of the four last touched the state. Result:
   stale styling, two competing "thinking" indicators, pills that
   never flush, etc.

3. **`run_tui_repl` is 2,000 lines of imperative `tokio::select!`.**
   Adding a feature means burrowing a new `if let Some(...)` chain
   somewhere in the giant loop. Nothing is testable in isolation; the
   only way to verify end-to-end behaviour is to launch the binary and
   eyeball the terminal — which is exactly how regressions keep
   landing.

4. **Tests cover units, never turns.** We snapshot single cells but
   never the full sequence
   `ThinkingStarted → ThinkingChunk×N → ThinkingStopped → Token×N →
   ToolStarted → ToolCompleted → Token×N → TurnComplete`. That's why
   nine green tests can coexist with a broken thinking preview.

What Codex (`/home/xupeng/github/codex/codex-rs/tui`) and Claude Code
(`/home/xupeng/claudecode`) actually do differently is in §3.

---

## 2. Target design — one paragraph

The TUI becomes **a stream of `HistoryCell` trait objects owned by a
single `ChatWidget`**, exactly like Codex. Every screen write goes
through one choke point. Streaming answers and reasoning live in a
**single `active_cell: Option<Box<dyn HistoryCell>>` slot** that is
converted into a committed `Arc<dyn HistoryCell>` on finalize.
Persistence is **structured per-cell JSONL** (like Claude Code), not
flattened `Line`s. Resume replays cells through the same renderer
that built them originally, so the output is pixel-identical, not a
lossy reprint. Event handling is an **imperative loop** (Codex-style
— the reducer framing was wrong for this workload), but we cap any
single file at ~800 lines and require every `AppEvent` variant to
route through one `handle_event` switch.

---

## 3. Reference audit — what Codex / Claude Code do, verbatim

From the reconnaissance pass on both codebases (under 800 words, one
code quote per question).

### 3.1 Source-of-truth for scrollback

**Codex.** `Vec<Arc<dyn HistoryCell>>` is the only canonical store.
`chatwidget.rs:748..762` carries `active_cell: Option<Box<dyn HistoryCell>>`
for the currently-streaming cell; on finalize, it's moved into the
committed vec. **No parallel transcript.** Ctrl+O scrolls the same
vec. Resume is "re-emit the same cells via `InsertHistoryCell(cell)`
events from the app-server."

**Claude Code.** `messages: Message[]` in a React store. Each message
is a structured object (user / assistant / tool / system union), not a
`Line`. Streaming mutates the last message in-place. Persisted as
JSONL at `~/.claude/projects/{project}/thread-{id}.jsonl`. On resume:
parse JSONL → push into the same store → components re-render.

**What we'll do.** Claude Code's model, Codex's types. One
`Vec<Arc<dyn HistoryCell>>` in memory, one JSONL per session on disk
using a `TurnEvent` enum (user / assistant / tool / thinking / system /
turn-summary). The renderer lives in the cell — `HistoryCell` has both
`display_lines(width) -> Vec<Line>` and `fn into_persist() -> TurnEvent`.

### 3.2 Event loop shape

**Codex.** Imperative `tokio::select!` in `app.rs:995..1043`. No
reducer, no Action/Effect split. ~11k lines in `chatwidget.rs`. They
survive that size by heavy `impl ChatWidget { fn on_x(...) }` splitting
and zero nesting in the select itself.

**What we'll do.** Match Codex: an imperative loop. I was wrong to
push reducers — the async HTTP streams plus direct terminal IO don't
map cleanly. But we cap files: `tui/mod.rs` ≤ 300 lines (just the
loop), all event handlers in `chatwidget/` one-per-file. Task #1 in
the existing task list ("migrate to reducer") is officially **wontfix**
and should be closed.

### 3.3 Streaming text + reasoning

**Codex.**
- Live streaming text lives in `StreamController.raw_source: String` +
  `rendered_lines: Vec<Line>`, NOT in `active_cell` until finalize.
- Reasoning is a **regular `ReasoningSummaryCell`** (history_cell.rs:
  466..536) with a `transcript_only: bool` flag. When `false` it
  renders in viewport with dim italic `"• "` prefix. No bordered
  window. No collapsed pill. It's just another cell.
- "Working" is a **separate `StatusIndicatorWidget`** rendered in the
  bottom pane above the composer, not in scrollback.

**Claude Code.** Streaming mutates the last message in place. No
visual distinction between "streaming" and "committed" — just a
cursor glyph. Reasoning is rendered if the model provides it, as
italic above the answer; nothing fancy.

**What we'll do.** Adopt Codex wholesale. Kill the framed thinking
window (it was my invention); reasoning becomes a plain
`ReasoningCell` with the same accent-gutter treatment as
`AssistantCell`, just dimmed. "Working" becomes a `StatusIndicator`
struct rendered by `BottomPane`, separate from scrollback, with the
elapsed + token counter we already have.

### 3.4 Persistence + resume

**Codex** does not persist locally — it fetches from its app-server
on resume. Not applicable to us directly (astra has a session journal
already at `~/.astra/sessions/<sid>.jsonl`).

**Claude Code** persists structured messages to JSONL, one message per
line, union-typed. On resume it re-inflates and the normal renderer
handles the rest.

**What we'll do.** Extend the existing session journal with a parallel
`~/.astra/transcripts/<sid>.jsonl` of `TurnEvent`s — structured, not
rendered Lines. On resume, we iterate events, call each cell's
constructor, push into the `Vec<Arc<dyn HistoryCell>>`, and the first
draw paints them. This kills `transcript_store.rs` (flattened-Line
format) and consolidates onto one authoritative store.

### 3.5 Tests

**Codex.** `tests/suite/vt100_history.rs` renders cells through a
`VT100Backend`, snapshots the ANSI bytes. `chatwidget/tests/` injects
events into a harness-built `ChatWidget` and inspects emitted
`InsertHistoryCell` events.

**What we'll do.** Mirror both patterns:
- `turn_driver.rs` — harness that drives a full SSE-like sequence
  into `ChatWidget` and asserts on the committed `Vec<HistoryCell>`.
- `vt100_snapshots.rs` — render cells to a `TestBackend`, assert
  ANSI-byte snapshots for every canonical cell + a full turn.

---

## 4. Concrete module layout (target)

```text
tui/
├── mod.rs                       ~250 lines: loop + do_draw only
├── app_event.rs                 AppEvent enum (TuiEvent, AgentEvent)
├── chat_widget/
│   ├── mod.rs                   ChatWidget struct + handle_event
│   ├── user_submit.rs           SubmitInput → UserCell + turn kickoff
│   ├── stream.rs                Token / ThinkingChunk → active_cell
│   ├── tool.rs                  ToolStarted / ToolCompleted → ToolCell
│   ├── turn_complete.rs         Finalize + turn summary line
│   └── resume.rs                Replay from JSONL on startup
├── history_cell/                one file per variant
│   ├── mod.rs                   HistoryCell trait + registry
│   ├── user.rs                  UserCell
│   ├── assistant.rs             AssistantCell   (markdown-rendered)
│   ├── reasoning.rs             ReasoningCell   (dim italic body)
│   ├── tool.rs                  ToolCell        (running/ok/err)
│   ├── system.rs                SystemCell
│   └── turn_summary.rs          TurnSummaryCell
├── stream_controller.rs         Newline-gated markdown streaming
├── status_indicator.rs          "Working … (↓ 5.1k tokens)"
├── bottom_pane/                 (unchanged — already decent)
├── transcript_store.rs          JSONL per session (TurnEvent schema)
└── tests/
    ├── turn_driver.rs           E2E harness, deterministic
    ├── vt100_snapshots.rs       ANSI-byte golden files
    └── resume.rs                Round-trip: write → reload → snapshot
```

Delete in this refactor:
- `agent_message_cell.rs` → merged into `AssistantCell`
- `assistant_cell.rs` (current) — replaced by new `assistant.rs` +
  `reasoning.rs` split
- `orbiter_line` in `mod.rs` — moved into `StatusIndicator`
- `thinking_cell.rs` (already dead) — confirmed removal
- flattened `transcript` `Vec<Line>` — replaced by
  `chat_widget.history: Vec<Arc<dyn HistoryCell>>`
- `scrollback_push` choke point — unnecessary once the store is
  `HistoryCell`-shaped

---

## 5. The `HistoryCell` trait

```rust
pub(crate) trait HistoryCell: Debug + Send + Sync + Any {
    /// Rendered view for the viewport and for scrollback. Called on
    /// every frame the cell is visible, so must be cheap / already
    /// pre-rendered from source.
    fn display_lines(&self, width: u16) -> Vec<Line<'static>>;

    /// Structured persistence payload. `None` for transient cells
    /// (e.g. a WorkingIndicator temporary state) that shouldn't land
    /// in the journal. Default: persist as plain text.
    fn into_persist(&self) -> Option<TurnEvent>;

    /// True while the cell is still being written to (streaming
    /// tokens, thinking chunks, running tool). Finalize() flips it.
    fn is_live(&self) -> bool { false }

    /// Called exactly once when the cell transitions from
    /// live→committed. Snapshot timers, freeze caches.
    fn finalize(&mut self) {}

    /// Reflow on terminal resize. Default: no cache, display_lines
    /// handles width internally.
    fn on_resize(&mut self, _width: u16) {}
}
```

The `TurnEvent` JSONL schema (one line per event, same directory as
the existing session journal for grep-ability):

```jsonc
{"kind":"user","ts":"2026-05-09T12:00:00Z","text":"make a plan"}
{"kind":"thinking","ts":"…","text":"user wants X, so…","duration_ms":3120}
{"kind":"assistant","ts":"…","markdown":"Here is the plan:\n…"}
{"kind":"tool","ts":"…","name":"bash","status":"ok","duration_ms":42,
 "description":"ls /tmp","output_summary":"3 entries"}
{"kind":"system","ts":"…","level":"info","text":"session resumed"}
{"kind":"turn_summary","ts":"…","elapsed_ms":16600,"ttft_ms":1757,
 "tokens_in":23200,"tokens_out":408,"tools":2}
```

Resume = iterate events → call `CellKind::from_persist(event)` → push
into `history`. Ctrl+O = render the full history to an overlay. No
separate flattened store.

---

## 6. Event flow (canonical)

```
                user types
                    │
                    ▼
           SubmitInput(text)
                    │
   ┌────────────────┼────────────────┐
   ▼                ▼                ▼
  1. history.push(UserCell)
  2. persist(TurnEvent::User)
  3. start turn via agent runtime
                    │
                    ▼
            AgentEvent stream
              (from app)
                    │
   ┌─────────────── / ──────────────────────┐
   │              │                 │       │
   Thinking       Token        ToolStarted  TurnComplete
   │              │                 │       │
   ac = active   ac = active    ac = ToolCell   finalize(ac) →
   ReasoningCell AssistantCell                  history.push(Arc::new(ac))
                                                persist(TurnEvent::Turn…)
                                                emit turn summary
```

Key invariants, enforced in tests:

- **At most one `active_cell` at a time.** Transition is always
  `active_cell = Some(new)` after `finalize()` moves the old one to
  `history`. No overlapping streams.
- **Every `history.push` is mirrored to persist.** There is exactly
  one function that adds to `history`, and it writes to JSONL in the
  same call. Impossible to drift.
- **`StatusIndicator` is never part of `history`.** It reads from
  `ChatWidget` state (turn elapsed, stream tokens, current tool) and
  renders in the bottom pane. Ephemeral by construction.

---

## 7. Migration plan (the part I want review on most)

Two-branch strategy. **Main stays green the entire time.**

### Phase 0 — freeze current behaviour (½ day)

- Create `enhance_tui_refactor` branch at current HEAD.
- Cherry-pick only these three "keep" commits onto a pristine
  `refactor_base` branch off `bcb872d5d`'s parent:
  - `bcb872d5d` persistent composer history + Ctrl+U (only the
    history parts; orbiter goes away in Phase 3)
  - `5539dd742` edge auth revert
  - `d1cfb0f3a` backtick strip (just the `Event::Code` lines)
- `refactor_base` becomes the merge target for refactor work.

### Phase 1 — `HistoryCell` + persistence skeleton (2 days)

- Introduce `TurnEvent` enum and JSONL reader/writer.
- Introduce `HistoryCell` trait in its final form.
- Build `ChatWidget` as a struct that owns
  `history: Vec<Arc<dyn HistoryCell>>`. **Not yet wired into the
  loop.** Compiles and has unit tests.
- `turn_driver.rs` harness lands here — it'll grow with each cell
  type.

### Phase 2 — rebuild cells one by one (3 days)

Each cell is its own commit with snapshot tests:

1. `UserCell` (simplest, validates the trait shape).
2. `SystemCell`.
3. `ToolCell` (includes running/ok/err states).
4. `AssistantCell` (markdown streaming via `StreamController`).
5. `ReasoningCell` (just `AssistantCell` with dim-italic style).
6. `TurnSummaryCell`.

After each commit, `cargo nextest` must be green; no partial cell.

### Phase 3 — swap the event loop (2 days)

- Replace current `run_tui_repl` body with a thin loop that calls
  `chat_widget.handle_event(event)`.
- `StatusIndicator` replaces `orbiter_line`.
- Delete the old `active_cell`, `transcript`, `scrollback_push`,
  `transcript_store` (Line-flattened).
- End-to-end `turn_driver` test asserts a canonical turn renders
  byte-identically to a golden snapshot.

### Phase 4 — resume replay (1 day)

- `chat_widget::resume::load(sid) -> Vec<Arc<dyn HistoryCell>>`.
- First-frame draw paints the loaded history.
- Golden test: write a turn, restart, `load`, snapshot the vt100
  output, compare.

### Phase 5 — delete the dead code (½ day)

All files listed in §4 "Delete in this refactor", plus any dangling
tests. CI must stay green.

Total: **~9 working days** for a clean cut.
Drop-dead if scope explodes: revert to `refactor_base` — main was
never broken.

---

## 8. Non-goals

- No change to `BottomPane` / composer / slash-menu / mention-menu
  code. They're fine.
- No change to theme, shimmer, markdown syntax-highlighting, diff
  renderer, table renderer. All kept as-is.
- No change to the agent runtime or SSE dispatch. Only the TUI
  consumes events differently.
- Not solving the "MiniMax doesn't emit reasoning deltas" problem.
  That's a server concern. If no `reasoning_delta` events arrive,
  the reasoning cell simply doesn't render, and that's correct.

---

## 9. Open questions — answer these and I can start

1. **Keep or drop the table renderer in its current form?** Codex
   doesn't emit tables from assistant streams — they go through a
   separate `/table` command. Ours tries to stream pipe-tables
   inline. The `hold-until-finalize` logic works but is fragile.
   Option A: keep as-is. Option B: strip to plain rows (match Codex).

2. **Session id at startup — read from where?** Today it's set
   async via the server. Resume loading needs `sid` *before* any
   cells render. Either (a) block briefly on `sid` before the first
   draw, or (b) render a placeholder first line and inject the
   history when `sid` lands. Codex does (a) — cleaner.

3. **Cap on history size.** Should `ChatWidget.history` be bounded?
   Codex caps to a session; we could do the same (GC the `Arc`s
   once rendered). Claude Code keeps everything. Vote?

4. **Do we keep `AgentMessageCell`'s streaming-mini-cell trick, or
   adopt Codex's single-cell-with-growing-source model?** The
   mini-cell approach flushes to scrollback per commit tick; Codex
   re-renders the whole stream into one cell. Ours uses less RAM;
   Codex is simpler and has no newline-gated quirks. I prefer
   Codex's model for the rewrite but flagging it.

5. **Is there any feature in the last 10 commits I shouldn't drop?**
   My proposed list: keep history persistence (in new format), keep
   backtick strip, keep edge auth revert, keep composer-history +
   Ctrl+U. **Drop everything else** — framed thinking window,
   collapsed pill, rotating star, token counter in orbiter, etc.
   Rebuild only the subset Codex/CC actually have.

---

## 10. Appendix — what gets deleted, commit-by-commit

| Commit       | Keep? | Reason                                               |
|--------------|-------|------------------------------------------------------|
| `d1cfb0f3a`  | 🟢 partial | Backtick strip is good; transcript guard goes away |
| `dc797f7bf`  | 🔴    | transcript_store (flattened Lines) — replace        |
| `0a1142667`  | 🔴    | thinking visibility patch on top of wrong design     |
| `5539dd742`  | 🟢    | edge refresh revert — correct as-is                  |
| `aac72a315`  | 🔴    | framed thinking window — Codex/CC don't have this    |
| `073aca061`  | 🔴    | pill-collapse hardening of a thing we're deleting    |
| `efbf70ce2`  | 🔴    | live thinking preview — ditto                        |
| `54e11d71d`  | 🟡 cherry | "calm orbiter" tweaks — fold into StatusIndicator |
| `eb1eb23c2`  | 🟡 cherry | token counter — fold into StatusIndicator         |
| `befe51c8b`  | 🔴    | single-indicator patch of a design we replace        |
| `bcb872d5d`  | 🟢 partial | composer history + Ctrl+U stay; orbiter goes     |

---

## 11. Ask

Please comment inline (or reply) on:

- §7 Migration plan — are phases sized right?
- §9 Open questions — especially Q4 (streaming model) and Q1 (tables).
- Any part of §5 `HistoryCell` trait you'd shape differently.

Once answered, I'll branch `refactor_base` and start Phase 1. No
code touched until then.
