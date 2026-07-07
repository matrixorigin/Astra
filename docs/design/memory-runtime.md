# Memory Runtime — End-to-End Architecture

> Current as of: `enhance_tool` branch (2026-05-12)
> Applies to: `astra-tools`, `astra-runtime`, `astra-turn-types`, `astra-prompts`
> Status: In production (PR against `main`)

This document describes how Astra's runtime uses Memoria — what is stored,
when it's stored, how it reaches the model, how it's updated, and how the
session lifecycle closes the loop. It supersedes the Python-era
`docs/design/memory/` and `docs/implementation/memory-*.md`.

For the narrow question of *what the LLM sees as a tool*, see the
`memory` tool schema in [`astra-tools/src/schemas.rs`](../../crates/astra-tools/src/schemas.rs).
For the session compaction / anchor / L1a-b layering, see
[session-memory-protocol.md](session-memory-protocol.md) — that layer is
*upstream* of this one (it handles in-session context; this doc handles
cross-session memory).

---

## 1. Architecture at a glance

```
 ┌── session start ──────────────────────────────────────────────────┐
 │                                                                   │
 │  prefetch_session_start_memories (memory_prefetch.rs)             │
 │    ├─ profile      query: "user profile preferences role"         │
 │    ├─ episodes     query: "recent session episode summary"        │
 │    └─ scenes       query: "recurring scene pattern insight"       │
 │                                                                   │
 │          │  session-stable block                                  │
 │          ▼                                                        │
 │  <memory_index>  (ambient awareness — gated by                   │
 │                   ASTRA_MEMORY_INDEX_INJECT=1)                   │
 │  <session_memory>  (profile / episodes / scenes, bucketed         │
 │                     freshness)                                    │
 │          │  pushed into stable_sections (CacheScope::Session)     │
 │          ▼                                                        │
 │  canonical SEEN_STORE (astra-tools) records every surfaced        │
 │  content-key and memory_id (one store for both paths)             │
 └───────────────────┬───────────────────────────────────────────────┘
                     │
 ┌── every turn ─────┴───────────────────────────────────────────────┐
 │  prefetch_memories (hybrid recall):                               │
 │    ├─ full-message query  ┐                                       │
 │    ├─ entity-token query  │  merge + sort by retrieval_score      │
 │    └─ cap at top_k × 2    │                                       │
 │                                                                   │
 │  filter_entries_already_surfaced → drop contents in SEEN_STORE   │
 │                                                                   │
 │  pushed into dynamic_sections (volatile lane) as "## User Memories"│
 │                                                                   │
 │  ── LLM-driven memory(action=...) ──                              │
 │  • recall  → decorate_recall_response adds freshness suffix       │
 │              + filters against process-global SEEN_STORE          │
 │              + pushes memory_ids onto RECALL_LEDGER for feedback  │
 │  • remember → detect_remember_conflict redirects duplicates       │
 │              to update before writing                             │
 │  • update/forget → REQUIRE reason + auto-snapshot (AFTER param    │
 │                    validation so rejects don't leave orphans)    │
 └───────────────────┬───────────────────────────────────────────────┘
                     │
 ┌── session end (debounced 15 min) ─────────────────────────────────┐
 │  post_loop_memory_cleanup (run_lifecycle.rs)                      │
 │    debouncer.should_run(session_id)?                              │
 │      ├─ Yes: run_session_end_governance                           │
 │      │        1. purge_working  ← session_id + memory_types filter│
 │      │        2. store_episode  ← deterministic SessionFacts      │
 │      │        3. reflect (mode=candidates) + store_scene for each │
 │      │      drain RECALL_LEDGER → `feedback(id, "useful")` per id │
 │      │      IFF episode was written (productive session)          │
 │      │      debouncer.record(session_id)                          │
 │      └─ No:  skip — next terminal run will try again              │
 │    Always:                                                        │
 │      • MemoriaClient::reset_seen (canonical seen-store)           │
 │      • MemoriaClient::reset_recall_ledger                         │
 │      • MemoryExtractionService::forget_session                    │
 └───────────────────┬───────────────────────────────────────────────┘
                     │
 ┌── background extraction (per turn) ───────────────────────────────┐
 │  memory_extraction.rs                                             │
 │    selector model proposes candidates                             │
 │    per-candidate top-3 recall → classify_write:                   │
 │      • Store  → POST /v1/memories                                 │
 │      • Update → PUT /v1/memories/{id}/correct                     │
 └───────────────────────────────────────────────────────────────────┘
```

Both `create_run` (non-streaming) and `stream_chat` (SSE, what the TUI
uses) call `post_loop_memory_cleanup` at terminal-run time. TUI sessions
therefore produce episodes + forward-fed scenes just like one-shot runs.

---

## 2. Data flowing through the system

### 2.1 Memory types Memoria stores

| Memoria `memory_type` | Astra category | Trust tier | Half-life | Primary author |
|---|---|---|---|---|
| `profile`   | `user`      | T1 (365d) | 365d | Background extraction / explicit `remember` |
| `semantic`  | `feedback`  | T2 (180d) | 180d | Background extraction / explicit `remember` |
| `semantic`  | `project`   | T3 (60d)  | 60d  | Background extraction / explicit `remember` |
| `semantic`  | `lesson`    | T3 (60d)  | 60d  | Background extraction |
| `semantic`  | `scene`     | T4 (30d)  | 30d  | **Reflect forward-feed** (P7) |
| `procedural`| `ref`       | T2 (180d) | 180d | Background extraction / explicit `remember` |
| `episodic`  | `episode`   | T3 (60d)  | 60d  | **Session-end governance** |
| `working`   | —           | —         | —    | Orchestrator scratch-pad / compaction context (purged at session end) |

Tags include `astra:<category>` and, for team memories, `astra:team:<id>`.
Scene memories are tagged `astra:scene`.

### 2.2 Freshness labels

`MemoriaMemory::freshness_suffix()` (and the parallel
`RankableMemory::freshness_suffix()`) routes through a shared pure
helper `astra_turn_types::freshness_suffix_for(days, trust_tier)`.
Labels are **bucketed**, not exact-day, so the prompt cache is stable
across midnight UTC:

| Age vs half-life | Suffix |
|---|---|
| `≤ 1 day` | empty (fresh — trust it) |
| `≤ 7 days` | ` (this week)` |
| `> 7d, ≤ half-life` | ` (within the year)` / ` (within the half-year)` / etc. per tier |
| `> half-life` | ` (stale — verify first)` |

The LLM treats `stale — verify first` as a decision boundary: verify the
claim against the live repo / grep before citing, and call
`memory(action=update, memory_id=..., reason=...)` if reality disagrees.

### 2.3 Cache lanes

- **stable lane** (`CacheScope::Session`, `PromptSection::stable`):
  `<memory_index>` + `<session_memory>`. Byte-stable for the whole
  session — deterministic sorting, bucketed freshness, no timestamps.
- **volatile lane** (`PromptTokenBucket::Environment`,
  `PromptSection::dynamic`): per-turn hybrid recall as
  `## User Memories`. Drifts every turn, lives post-cache-marker.

Prior to the `enhance_tool` branch these shared one combined section
that was silently dropped whenever per-turn recall had entries — the
session-start profile + episode prewarm disappeared on exactly the
sessions that had enough memory to benefit. Fixed in
`bridge_inprocess.rs`.

---

## 3. The `memory` tool

Single tool, nine cognitive verbs. See the schema at
`astra-tools/src/schemas.rs::memory`.

| Action | Routes to | Required args | Notes |
|---|---|---|---|
| `remember` | `POST /v1/memories` | `content` | Runs `detect_remember_conflict` first; near-duplicates (score ≥ 0.85) return a `status: conflict, action_required: update` envelope instead of writing. |
| `recall` | `POST /v1/memories/retrieve` | `query` | Response is post-processed by `decorate_recall_response`: freshness suffixes appended per entry, memory_ids already shown this session dropped. |
| `expand` | `GET /v1/memories/{id}` | `memory_id` | |
| `update` | `PUT /v1/memories/{id}/correct` or `POST /v1/memories/correct` | `memory_id` OR `query`, plus **non-empty `reason`** | Runtime rejects missing/blank reason with a structured error; schema also enforces via `if/then` conditional. Auto-snapshots `pre_update_{ts}` before dispatch. |
| `forget` | `POST /v1/memories/purge` | `memory_id`/`memory_ids`/`topic`, plus **non-empty `reason`** | Same reason + snapshot contract as `update`. Snapshot name: `pre_forget_{ts}`. |
| `focus` | in-process (TTL store) | `focus_type`, `focus_value` | Next `recall` in the same session folds boost hints into the request payload. |
| `reflect` | `POST /v1/reflect` | — | Governance uses `mode=candidates` (P7); LLM-driven reflect uses default. |
| `profile` | `GET /v1/profile/{user}` | — | |
| `feedback` | `POST /v1/memories/{id}/feedback` | `memory_id`, `signal` | Signals: `useful` / `irrelevant` / `outdated` / `wrong`. |

### 3.1 Visibility

```
memory(action=remember, content=..., visibility="team", team_id="X")
```

Adds `astra:team:X` to the tag set. On `recall` with `visibility=team`,
`include_tags: ["astra:team:X"]` is forwarded and the server unions
team-tagged hits into the result. `team_id` must match
`^[A-Za-z0-9_-]{1,64}$`; missing `team_id` on `visibility=team` is
rejected loudly.

### 3.2 Auto-snapshot safety net

Destructive verbs (`forget`, `update`) call
`memoria_snapshot_create(pre_<op>_<ms>)` before the HTTP op. A snapshot
failure logs at `warn` and continues — a misconfigured snapshot service
must not block corrective actions. Recovery is via
`memory(action=rollback, name=pre_forget_1700...)` or the admin CLI.

---

## 4. Write paths

### 4.1 LLM-driven `remember`

```
tool call → MemoriaClient::call_with_timeout
           → detect_remember_conflict (2s, top-3 recall)
             ├─ hit above 0.85  → return conflict envelope (do NOT write)
             └─ no hit          → build_direct_request → POST /v1/memories
```

The conflict envelope carries the existing `memory_id` and a
`retry_hint` telling the model to call `memory(action=update,
memory_id=..., reason=...)` instead.

### 4.2 Background extraction (`memory_extraction.rs`)

Runs after each substantive turn that the main model didn't already
touch memory on. For every quality-filtered candidate:

1. 2s top-3 recall on the candidate's encoded content.
2. `astra_tools::memoria::classify_write(&candidates)`:
   - no hit ≥ 0.85 → `WriteDecision::Store` → `POST /v1/memories`
   - hit ≥ 0.85 → `WriteDecision::Update { memory_id }` →
     `PUT /v1/memories/{id}/correct` with reason
     `"session-end extraction refinement"`
3. Failures (network, HTTP) fail open to `Store` so we don't drop
   extractions silently. The `[memory-extraction]` log line reports
   `N new, M updated`.

The prior code batch-POSTed every candidate unconditionally — refinements
of existing memories thus accumulated as duplicates.

### 4.3 Session-end governance (`session_end_governance.rs`)

Runs exactly once per session per debounce window (default 15min, see
`session_end_debounce.rs`). Three deterministic steps:

1. **`purge_working(session_id)`** — uses
   `{"session_id": "...", "memory_types": ["working"]}` selector, NOT
   topic-based fulltext (UUID tokens never matched the ngram tokenizer).
2. **`store_episode(session_id, overview)`** — overview is a pure
   function of `SessionFacts`: turn count, token estimate, files
   touched, tool ok/fail tally, last error (capped at 120 chars).
   Skipped for trivial sessions.
3. **`reflect_session(mode=candidates)`** → for each
   `ReflectCandidate` → **`store_scene(session_id, signal, summary)`**.
   Scene memories are `semantic` with content prefixed `[scene:<signal>]`
   and tagged `astra:scene`. This is the **loop-closure** step — reflect
   output used to be discarded; now it feeds the next session's prewarm.

---

## 5. Read paths

### 5.1 Session-start prewarm (turn 1 only)

`prefetch_session_start_memories` runs three Memoria queries in parallel:

| Bucket | Query | Client-side filter |
|---|---|---|
| profile | `"user profile preferences role"` | `memory_type == "profile"` |
| episodes | `"recent session episode summary"` | `memory_type == "episodic"` |
| scenes | `"recurring scene pattern insight"` | `memory_type == "semantic" && content.starts_with("[scene")` |

Each bucket is sorted by server-side `retrieval_score`, truncated to 3,
and rendered into `<session_memory>` with freshness suffixes.

### 5.2 `<memory_index>` (ambient awareness)

Currently **off by default** behind `ASTRA_MEMORY_INDEX_INJECT=1`.
Produces a compact `- [type] memory_id: abstract` listing of up to 80
memories. Sorted by `memory_id` and deduped against `<session_memory>`
ids so the two blocks don't repeat the same content. Once on, the
cost is bounded and the signal is the full recall surface the LLM
*could* hit via `memory(action=recall, query=X)`.

### 5.3 Per-turn hybrid recall (every turn)

`prefetch_memories` runs two queries (full user message + entity tokens)
in parallel, merges by memory_id, sorts by retrieval_score, caps at
`top_k × 2`. Each entry passes through:

- `compact_view_of` — strips overview/detail layers to a single abstract
- `is_memory_worthy` — rejects known noise shapes (session-replay
  echoes, L1 protocol markers, runtime scaffolding prefixes)
- `memory_dedup_key` — case-fold + trailing-punctuation strip
- `filter_entries_already_surfaced` — drops contents the canonical
  `SEEN_STORE` (see §7) already surfaced via `<session_memory>` or a
  prior recall

### 5.4 LLM-driven `recall`

`decorate_recall_response(raw_text, seen_ids, &mut newly_surfaced)`:

1. Parses the top-level array (passes through verbatim for error
   envelopes / malformed JSON).
2. Drops entries whose `memory_id` is in `seen_ids`.
3. For each survivor: computes `days_since(observed_at || updated_at)`,
   appends `freshness_suffix_for(days, trust_tier)` to the `content`
   field.
4. Records surviving ids in the canonical `SEEN_STORE` so the next
   recall in the same session filters them out.
5. **Pushes surviving ids onto the RECALL_LEDGER** so session-end can
   attribute a `useful` feedback signal if the session is productive.

---

## 6. Session lifecycle

```
┌─────────────────────────────────────────────────────────────┐
│ per-turn activity:                                          │
│   • hybrid recall → <## User Memories>                      │
│   • LLM may call memory(action=...) (recall/remember/...)   │
│   • extraction writes candidates (async, debounced)         │
└──────────────────────┬──────────────────────────────────────┘
                       ▼
      terminal run of the session? (create_run / stream_chat)
                       │
                       ▼
        post_loop_memory_cleanup (run_lifecycle.rs)
                       │
                       ├── session_end_debounce.should_run()
                       │      │
                       │      ├── Run:
                       │      │    1. purge_working (session_id filter)
                       │      │    2. store_episode
                       │      │    3. reflect → store_scene per candidate
                       │      │    4. drain RECALL_LEDGER → feedback(
                       │      │       id, "useful") per memory_id
                       │      │       (conditional on episode_was_written)
                       │      │    debouncer.record(session_id)
                       │      │
                       │      └── Skip (recent governance run):
                       │           log at debug, no-op
                       │
                       ├── MemoriaClient::reset_seen (canonical)
                       ├── MemoriaClient::reset_recall_ledger
                       └── MemoryExtractionService::forget_session
```

### 6.1 Why the debouncer

Session IDs are sticky across many `create_run`s (user reopens a
session, TUI issues follow-up turns). Without the debouncer every
terminal run wrote an episode and hammered reflect — the
`recent_episodes` prefetch pool filled with fragments of a single
session and the backend's 1h reflect cooldown became the only brake.
The client-side debouncer (15min default) gives episodes time to
batch and avoids wiping mid-conversation working memory when the user
resumes.

### 6.2 Streaming vs non-streaming

Before `enhance_tool` only `create_run` called governance.
`stream_chat` (the SSE path that the TUI uses) didn't — so TUI
sessions **never produced episodes**, and the next session's
`<session_memory>` prewarm stayed empty forever. Fixed by extracting
the cleanup into a helper both paths call after their event-tx drop.

---

## 7. Process-global session-state stores

Two canonical process-global stores live in `astra-tools::memoria`.
Both are keyed by `session_id` and both are cleared by
`post_loop_memory_cleanup` at session end so a long-lived server
doesn't accumulate per-session state forever. Both survive the
per-call `MemoriaClient` / `HttpMemoriaClient` instance lifetime
(Memoria clients are constructed per-tool-call in
`server_tool_executor.rs` and `edge_tools/memoria.rs`).

| Store | API | Tracks | Used by |
|---|---|---|---|
| `SEEN_STORE` | `MemoriaClient::{record_seen, seen_snapshot, reset_seen}` | Union of (a) content dedup keys for entries already shown in `<session_memory>` / `## User Memories`, (b) memory_ids returned by LLM-driven `memory(action=recall)`. | Bridge prefetch + `decorate_recall_response` + `MemoryOrchestrator` (as delegating facade) |
| `RECALL_LEDGER` | `MemoriaClient::{record_recall, drain_recalls, pending_recall_count, reset_recall_ledger}` | FIFO queue of `RecallSnapshot` (session_id, memory_ids, turn, at) per session, soft-capped at 16. | `decorate_recall_response` pushes; `post_loop_memory_cleanup` drains at session-end and routes `useful` feedback if episode was written. |

The runtime-side `MemoryOrchestrator` (`turn/cloud/memory_orchestrator.rs`)
is a thin delegating facade over the same stores — its
`mark_surfaced` / `filter_already_surfaced` / `reset_session_surface`
methods call through to `MemoriaClient::{record_seen, seen_snapshot,
reset_seen}`. No parallel state lives in the orchestrator.

---

## 8. The prompt

The auto-memory prompt (`astra-prompts/src/memory_types.rs`) tells the
LLM how to decide when to store, when to update, and how to read
freshness. Key rules post-`enhance_tool`:

- **Explicit-save gate**: store only when the user stated something
  durable; favor false negatives (silence cheaper than noise). The
  prior `"just store, then confirm"` phrasing is gone.
- **Why / How body structure** for `feedback` and `project`:
  rule/fact → `**Why:**` (the reason or incident) →
  `**How to apply:**` (when/where the rule kicks in).
- **Update over store+purge**: if the conflict gate redirects to an
  existing memory, call `memory(action=update, memory_id=..., reason=...)`.
- **Destructive ops require a non-empty `reason`** — the runtime
  rejects missing / blank reasons before dispatch.
- **Freshness vocabulary**: `(this week)`, `(within the year)`,
  `(stale — verify first)`. No exact-day labels (those caused daily
  cache churn).

---

## 9. Environment variables

| Variable | Default | Effect |
|---|---|---|
| `MEMORIA_MASTER_KEY`, `MEMORIA_BASE_URL` | — | Direct Memoria access (required when cloud proxy not configured). |
| `ASTRA_MEMORY_INDEX_INJECT` | `0` | Set to `1` / `true` to inject `<memory_index>` on turn 1. |
| `ASTRA_PIPELINE_DUMP_SYSTEM_PROMPT` | — | Dump assembled system prompt to `$TMPDIR/astra-bridge-prompt-<sid>-<ts>.json` for cache-diff inspection. |

---

## 10. Common questions

**Q: The LLM called `memory(action=forget)` but nothing was deleted.**
Check the response envelope — the most likely cause is a missing or
blank `reason`, which the runtime rejects with
`"memory(action=forget) requires a non-empty reason (audit trail)"`.

**Q: Can I recover from a bad `forget`?**
Yes. Every `forget` and `update` creates an auto-snapshot named
`pre_<op>_<ms>`. Call `memoria_snapshot_rollback(name=...)` or the
equivalent admin CLI.

**Q: Why doesn't my memory re-appear after writing it this turn?**
The per-turn recall query may not hit it (different semantic neighborhood).
Try `memory(action=recall, query="...", top_k=20)` — the seen-ledger
only filters ids previously surfaced, not ids you've written.

**Q: How do I know a memory is stale?**
The freshness suffix `(stale — verify first)` appears on any memory
whose age exceeds its trust-tier half-life (T1=365d, T2=180d,
T3=60d, T4=30d). Verify the claim against current state before
citing; if reality disagrees, call `memory(action=update, ...)`.

**Q: I want to share a memory with the team.**
```
memory(action=remember, content="...", visibility="team", team_id="my-team")
```
To retrieve team memories later:
```
memory(action=recall, query="...", visibility="team", team_id="my-team")
```

**Q: Is `<memory_index>` on?**
No — default is off. Set `ASTRA_MEMORY_INDEX_INJECT=1` to enable. When
on it occupies ~1200 tokens on turn 1 only and gives the LLM ambient
awareness of what it could recall.

---

## 11. Review-driven design choices

Lessons from multi-agent reviews on the `enhance_tool` branch that
shaped the current design.

**Meta-rules for future maintainers** — these are the anti-patterns
we fell into and the guardrails against them:

1. **Never create a parallel store to avoid touching an existing one.**
   When a new code path needs the same state some other path already
   tracks, _share the store_. Three parallel "already surfaced" sets
   is what we got from three successive "I'll just add another ledger
   to avoid touching X" decisions. Symptom to watch for: writing a
   comment like *"paired with the runtime-side `memory_seen_ledger`"*
   — that's the moment to stop and unify.
2. **Dead code left in tree is a maintenance trap, not a future hook.**
   `MemoryOrchestrator` sat un-wired for months while 3 other modules
   reimplemented its API half-heartedly. Either wire it or delete it;
   "leave it for later" produces the worst outcome.
3. **Destructive ops validate args *before* side effects, always.**
   Snapshot/log/commit before-the-guard creates orphans that look
   identical to real artifacts. The order is: validate → commit → act.
4. **No silent truncation in summary outputs.** If the full list
   mattered enough to collect, the fact of truncation matters enough
   to render. `(+N more)` / `…` / `[truncated]` — pick one.
5. **When a design note "justifies" a limit with an assumption, flag
   the assumption as a test.** "latest-only because older are probably
   acted on" was a guess. Write the test that proves or disproves it,
   or remove the limit.

Concrete applications of these rules:

- **One canonical dedup store, not three.** An earlier iteration had
  parallel "already surfaced" sets in the orchestrator, a runtime
  `memory_seen_ledger` module, and a tool-side `SEEN_STORE`. Every
  new site had to reset all three. Collapsed to one store in
  `astra-tools::memoria` that both the bridge (content keys) and the
  tool decorator (memory_ids) write to. The orchestrator is a
  delegating facade over that single store — no private state.
- **Auto-snapshot happens AFTER parameter validation.** A rejected
  `forget`/`update` no longer produces orphan `pre_<op>_*`
  snapshots that poll the snapshot service.
- **Episode truncation is loud, not silent.** When the files/tools
  list exceeds the cap, the rendered overview appends `(+N more)` so
  a reader can tell the summary is abbreviated.
- **Recall ledger is FIFO, not last-only.** The LLM may probe
  multiple times in a single turn before acting; every probe goes on
  the queue and all are scored at session-end. Soft-capped at 16 so
  the queue doesn't grow unbounded on sessions that never close the
  loop.
- **Feedback loop closes at session-end, conservatively.** The
  canonical RECALL_LEDGER is drained when `post_loop_memory_cleanup`
  runs. If the session produced an episode (substantive work
  happened) every recalled memory_id gets a `useful` feedback
  signal. Trivial sessions drop the snapshots without scoring to
  avoid false positives. A richer per-tool-outcome attribution is
  future work — this is the minimum viable loop-closure.

---

## 12. Pointers to source

| Concern | File |
|---|---|
| Tool schema (LLM-facing) | `crates/astra-tools/src/schemas.rs` |
| v2→v1 verb translation, conflict gate, decorator | `crates/astra-tools/src/memoria.rs` |
| Bridge-side prefetch + cache lanes | `crates/runtime/src/turn/bridge_inprocess.rs` |
| Pure prefetch helpers | `crates/runtime/src/turn/memory_prefetch.rs` |
| Canonical session stores (SEEN_STORE + RECALL_LEDGER) | `crates/astra-tools/src/memoria.rs` (`record_seen`, `seen_snapshot`, `reset_seen`, `record_recall`, `drain_recalls`, `reset_recall_ledger`) |
| Session-end governance + store_scene | `crates/runtime/src/turn/cloud/session_end_governance.rs` |
| HTTP client + store_scene + purge_working + freshness | `crates/runtime/src/turn/cloud/memoria_compact.rs` |
| Per-session debouncer | `crates/runtime/src/turn/session_end_debounce.rs` |
| Freshness helper (single source of truth) | `crates/astra-turn-types/src/memory_ranking.rs` |
| Background extraction with update-vs-store | `crates/astra-cli/src/cli/memory_extraction.rs` |
| Post-loop cleanup dispatcher | `crates/runtime/src/server/run_lifecycle.rs::post_loop_memory_cleanup` |
| Auto-memory prompt | `crates/astra-prompts/src/memory_types.rs` |
| Memoria orchestrator (facade, wired into bridge) | `crates/runtime/src/turn/cloud/memory_orchestrator.rs` |
