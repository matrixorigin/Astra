# Memory Runtime — End-to-End Architecture

> Current as of: `enhance_tool` branch (2026-05-12)
> Applies to: `astra-tools`, `astra-runtime`, `astra-turn-types`, `astra-prompts`
> Status: In production (PR against `main`)

This document describes how Astra's runtime uses Memoria — what is stored,
when it's stored, how it reaches the model, how it's updated, and how the
session lifecycle closes the loop. It supersedes the Python-era
`docs/design/memory/` and `docs/implementation/memory-*.md`.

For the narrow question of *what the LLM sees as a tool*, see the
`memory` tool schema in [`astra-tools/src/schemas.rs`](../../rust/crates/astra-tools/src/schemas.rs).
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
 │  seen_ledger records every surfaced memory_id (memory_seen_ledger)│
 └───────────────────┬───────────────────────────────────────────────┘
                     │
 ┌── every turn ─────┴───────────────────────────────────────────────┐
 │  prefetch_memories (hybrid recall):                               │
 │    ├─ full-message query  ┐                                       │
 │    ├─ entity-token query  │  merge + sort by retrieval_score      │
 │    └─ cap at top_k × 2    │                                       │
 │                                                                   │
 │  filter_entries_already_surfaced → drop memory_ids in seen_ledger │
 │                                                                   │
 │  pushed into dynamic_sections (volatile lane) as "## User Memories"│
 │                                                                   │
 │  ── LLM-driven memory(action=...) ──                              │
 │  • recall  → decorate_recall_response adds freshness suffix       │
 │              + filters against process-global SEEN_STORE          │
 │  • remember → detect_remember_conflict redirects duplicates       │
 │              to update before writing                             │
 │  • update/forget → REQUIRE reason + auto-snapshot before dispatch │
 └───────────────────┬───────────────────────────────────────────────┘
                     │
 ┌── session end (debounced 15 min) ─────────────────────────────────┐
 │  post_loop_memory_cleanup (run_lifecycle.rs)                      │
 │    debouncer.should_run(session_id)?                              │
 │      ├─ Yes: run_session_end_governance                           │
 │      │        1. purge_working  ← session_id + memory_types filter│
 │      │        2. store_episode  ← deterministic SessionFacts      │
 │      │        3. reflect (mode=candidates) + store_scene for each │
 │      │      debouncer.record(session_id)                          │
 │      └─ No:  skip — next terminal run will try again              │
 │    Always:                                                        │
 │      • memory_seen_ledger.reset_session                           │
 │      • MemoriaClient::reset_seen                                  │
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
| `working`   | —           | —         | —    | Orchestrator scratch-pad (purged at session end) |

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
- `filter_entries_already_surfaced` — drops memory_ids in the
  session's `memory_seen_ledger`

### 5.4 LLM-driven `recall`

`decorate_recall_response(raw_text, seen_ids, &mut newly_surfaced)`:

1. Parses the top-level array (passes through verbatim for error
   envelopes / malformed JSON).
2. Drops entries whose `memory_id` is in `seen_ids`.
3. For each survivor: computes `days_since(observed_at || updated_at)`,
   appends `freshness_suffix_for(days, trust_tier)` to the `content`
   field.
4. Records surviving ids in the process-global `SEEN_STORE` so the
   next recall in the same session filters them out.

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
                       │      │    debouncer.record(session_id)
                       │      │
                       │      └── Skip (recent governance run):
                       │           log at debug, no-op
                       │
                       ├── memory_seen_ledger.reset_session
                       ├── MemoriaClient::reset_seen
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

## 7. The seen-ledger (prevents re-injection)

Two companion ledgers, process-global, keyed by `session_id`:

| Ledger | Where | Tracks |
|---|---|---|
| `memory_seen_ledger` | `astra-runtime/turn/memory_seen_ledger.rs` | Content dedup keys (normalized) for entries shown in `<memory_index>`, `<session_memory>`, and the per-turn `## User Memories` block. |
| `SEEN_STORE` (static) | `astra-tools/memoria.rs` | `memory_id` set for LLM-driven `recall` responses. |

Both are cleared by `post_loop_memory_cleanup` at session end so a
long-lived server doesn't accumulate per-session state forever. Both
survive the per-call `MemoriaClient` / `HttpMemoriaClient` instance
lifetime (Memoria clients are constructed per-tool-call in
`server_tool_executor.rs` and `edge_tools/memoria.rs`).

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

## 11. Pointers to source

| Concern | File |
|---|---|
| Tool schema (LLM-facing) | `rust/crates/astra-tools/src/schemas.rs` |
| v2→v1 verb translation, conflict gate, decorator | `rust/crates/astra-tools/src/memoria.rs` |
| Bridge-side prefetch + cache lanes | `rust/crates/runtime/src/turn/bridge_inprocess.rs` |
| Pure prefetch helpers | `rust/crates/runtime/src/turn/memory_prefetch.rs` |
| Seen ledger | `rust/crates/runtime/src/turn/memory_seen_ledger.rs` |
| Session-end governance + store_scene | `rust/crates/runtime/src/turn/cloud/session_end_governance.rs` |
| HTTP client + store_scene + purge_working + freshness | `rust/crates/runtime/src/turn/cloud/memoria_compact.rs` |
| Per-session debouncer | `rust/crates/runtime/src/turn/session_end_debounce.rs` |
| Freshness helper (single source of truth) | `rust/crates/astra-turn-types/src/memory_ranking.rs` |
| Background extraction with update-vs-store | `rust/crates/astra-cli/src/cli/memory_extraction.rs` |
| Post-loop cleanup dispatcher | `rust/crates/runtime/src/server/run_lifecycle.rs::post_loop_memory_cleanup` |
| Auto-memory prompt | `rust/crates/astra-prompts/src/memory_types.rs` |
| Memoria orchestrator (facade, wired into bridge) | `rust/crates/runtime/src/turn/cloud/memory_orchestrator.rs` |
