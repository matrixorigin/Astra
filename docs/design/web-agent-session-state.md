# Astra Web Agent Session State and Context Design

> Status: Draft v0.1
> Date: 2026-05-06
> Owner: astra runtime / web agent
> Scope: cloud API server + browser web agent + MatrixOne-backed session state

## Intent

This document is the working design for adding a production web agent to astra.
It is intentionally broader than a schema proposal because the hard part is not
only storing session state. The hard part is deciding which slice of that state
the LLM should see on each turn, while still preserving enough durable state for
resume, audit, UI display, skills, plans, todos, and future features.

Primary goals:

- Deploy astra API server in the cloud and let users run agent sessions from the
  browser.
- Store all session state, including user personal skills, in MatrixOne and bind
  it to `user_id`.
- Resume the same session from another device without depending on local CLI
  files.
- Save tokens through deliberate context assembly, not by losing history.
- Keep the state model extensible for plans, todos, delegation, approvals,
  workspace artifacts, skill selection, and future state categories.

Non-goals for this design:

- Direct browser access to an arbitrary laptop repository. A web-only session
  needs a cloud workspace/sandbox. A local repository needs the existing
  edge-cloud execution path.
- Treating the stored transcript as the prompt. Full storage is an audit source;
  the prompt is a budgeted runtime projection.

## Current Completion

Overall readiness for a production web agent is about **45-55%**. The platform
has strong persistence and execution foundations, but the web product path and
the explicit context contract are incomplete.

| Area | Current state | Completion |
| --- | --- | --- |
| Auth and user-bound sessions | `auth_*`, `/auth/*`, `agent_sessions`, `/sessions` exist. | High |
| Browser workspace UI | `web/app/(dashboard)/workspace` has session sidebar, chat stream, tool timeline, plan and context side panels. | Medium |
| Cloud chat execution | `/chat`, `/chat/stream`, `/chat/turn`, `/chat/ws`; server-side tool execution exists when no edge tools are attached. | Medium |
| Cross-device LLM continuity | DB-backed CSL exists through `conversation_log`; `restore_csl_history()` reconstructs messages/state before new turns. | Medium |
| Run durability | `RunStateStore` and `RunEngine` exist, but server wiring still uses `InMemoryRunStateStore`. | Low |
| UI resume | Web session list exists, but prior transcript hydration from DB is missing; current chat messages are React-local. | Low |
| Context governance | `ctx_snapshots`, CSL compaction, session memory protocol docs exist, but there is no first-class per-turn context manifest used by web/session APIs. | Medium-low |
| User personal skills | `skills_registry`, installations, settings, credentials exist; web CRUD/version/source UX is not complete. | Medium-low |
| Approvals / interactive waits | Backend has pause/resume/cancel/run stream concepts; browser stream path does not yet model all interactive prompts and stop currently only closes SSE. | Low |

### Existing Strengths

- `rust/crates/services/src/storage.rs` already creates the core MatrixOne schema
  for sessions, events, context snapshots, skills, preferences, sync logs,
  plans, plan step runs, session checkpoints, artifacts, tasks, contracts, and
  auth.
- `rust/crates/astra-turn-core/src/conversation_log/*` implements an append-only
  conversation state log with snapshots and deltas. It can materialize the
  latest assistant context from `conversation_log`.
- `rust/crates/runtime/src/server/run_lifecycle.rs` restores CSL history for
  resumed sessions and persists CSL after a turn.
- `rust/crates/runtime/src/server/bridge_prep.rs` has a typed `/chat/turn`
  bridge protocol with turn identity, state-sync headers, edge tool caching,
  routing metadata, and execution state headers.
- `rust/crates/services/src/state_sync.rs` already treats cloud state as a sync
  source for learning snapshots, preferences, plans, and tasks.
- Existing design docs already state the right principle:
  `docs/design/context-window-management.md` says runtime context and audit
  snapshots are different objects; `docs/design/session-memory-protocol.md`
  defines an L0/L1/L2/L3 memory pyramid.

### Important Gaps

1. `RunEngine` is described as durable, but server state construction wires
   `InMemoryRunStateStore`. A cloud web agent cannot rely on this after process
   restart, load-balancer routing, or reconnect.
2. Web session resume is metadata-only. The UI can load a session row and recent
   events/reflection, but not hydrate the display transcript from
   `conversation_log` or a message projection endpoint.
3. Session state is split across `conversation_log`, `agent_events`,
   `ctx_snapshots`, `session_checkpoints`, `session_artifacts`, plans, tasks,
   preferences, and skills without a single state projection contract for the
   context builder.
4. There is no persisted `context_manifest` saying what state slices were
   included, how many tokens they used, what was dropped, and why. Without this,
   token-saving decisions are hard to debug or evolve.
5. Tool outputs and artifacts need a web-safe storage policy. Large outputs
   should be stored fully for audit/display, but the prompt should receive only
   summaries, previews, or references.
6. Personal skills need versioned user-owned content and web lifecycle
   operations, not only registry/install rows.
7. Plans and todos are not yet a first-class part of web resume and context
   assembly. They exist in separate systems but need stable projection into both
   UI state and LLM context.

## Reference Survey

Sources:

- Claude Agent SDK session storage docs:
  https://code.claude.com/docs/en/agent-sdk/session-storage
- Requested Claude Code mirror:
  https://github.com/weynechen/claude-code
  was inaccessible on 2026-05-06. `git clone` returned GitHub 403 with the
  repository disabled, so this design uses the official Claude docs and local
  astra comparison docs instead.
- opencode source inspected from `sst/opencode`:
  https://github.com/sst/opencode
- GitHub Copilot CLI session data docs:
  https://docs.github.com/en/copilot/concepts/agents/copilot-cli/chronicle
  and
  https://docs.github.com/en/copilot/reference/copilot-cli-reference/cli-config-dir-reference
- Local structural inspection of `~/.copilot/` and `~/.codex/`. No private
  prompt or transcript content was used.

### Claude Code / Claude Agent SDK

The SDK treats session storage as an abstraction. Default storage is local, and
external storage can implement the same session CRUD surface. This is the right
shape for astra: the agent loop should not know whether state lives in local
files, MatrixOne, S3, or a hybrid cache. For astra web, MatrixOne should be the
authoritative store and local CLI state should be a cache/sync peer.

Design lesson:

- Keep a storage interface boundary.
- Persist conversation history, metadata, and agent state together, but expose
  context assembly as a separate layer above storage.

### opencode

opencode is local-first with a server API and SQLite storage. Its session schema
has normalized rows for `session`, `message`, `part`, and `todo`. A message is
not a blob: each part can be text, reasoning, file, tool, retry, snapshot, patch,
subtask, or compaction marker. The API exposes session list/get, status, todo,
message pagination, prompt, abort, fork, share, summarize/compact, diff, revert,
and part update/delete.

Notable patterns:

- `session` stores project/workspace/session metadata, parent session, model,
  permissions, share URL, revert target, summary stats, and compacting/archive
  timestamps.
- `message` stores role-level metadata; `part` stores all display and execution
  parts with separate indexes.
- `todo` is normalized by `(session_id, position)`.
- Run status is process-local in the inspected source, so it has the same
  cloud-readiness gap astra must avoid.
- Compaction is explicit: a compaction user part starts the process, a summary
  assistant message is written, old completed tool outputs can be pruned, and a
  tail-turn budget preserves recent context.
- The TUI sync layer hydrates `session`, `messages`, `todo`, and `diff` as a
  client-side projection. This is close to what astra web should expose.

Design lesson:

- Store the display model as normalized message parts rather than a single raw
  prompt transcript.
- Give todos and diff/artifact state their own projection.
- Make compaction observable and addressable; do not hide it inside opaque
  prompt strings.

### GitHub Copilot CLI

GitHub documents Copilot CLI session data as local session directories under
`~/.copilot/session-state/`, with an event log and workspace artifacts, plus a
SQLite session store used for indexed/queryable subsets. The docs explicitly
call out `events.jsonl`, `workspace.yaml`, `plan.md`, `checkpoints/`, and
`files/`. Local inspection also showed per-session `session.db` tables for
`todos`, `todo_deps`, and `inbox_entries`.

Design lesson:

- Keep the full event log recoverable even when the indexed store is rebuilt.
- Store plan/checkpoint/file artifacts separately from the transcript.
- Normalize todos and dependencies because they are hot UI/workflow state.
- Use session history for self-improvement and search, but keep privacy and
  retention explicit.

### Codex

Local Codex state uses JSONL rollout files plus SQLite metadata. The inspected
SQLite tables include `threads`, `thread_goals`, `thread_spawn_edges`,
`thread_dynamic_tools`, `agent_jobs`, `agent_job_items`, `jobs`,
`device_key_bindings`, and structured logs. This separates the append-only
rollout from indexed thread/job/tool/goal metadata.

Design lesson:

- Transcript/event rollouts and operational indexes should be separate.
- Dynamic tool schemas belong to thread/session state because they affect future
  context and must be resumable.
- Spawn/delegation edges and job items need relational indexes; do not bury them
  inside a transcript JSON blob.

### Local Agent Storage Observations

I inspected local session stores under `~/.copilot`, `~/.codex`, `~/.claude`,
and `~/.kiro` to understand the concrete storage shapes. The useful patterns
are structural, not content-specific.

| Tool | Raw history | Structured state | Context/compaction signal | Lesson for astra |
| --- | --- | --- | --- | --- |
| Copilot CLI | Per-session `events.jsonl` with `{type,data,id,timestamp,parentId}`. Events include user/assistant messages, tool start/complete, plan changes, compaction start/complete, context changes. | `session.db` with `todos`, `todo_deps`, `inbox_entries`, sometimes `session_state` KV; `plan.md`; checkpoint files. | Compaction events store token counts, summary content, checkpoint number/path. | Strong example of transcript + plan/todo/checkpoints split. |
| Codex | Rollout JSONL with `session_meta`, `turn_context`, `response_item`, `event_msg`, `compacted`. | SQLite `threads`, `thread_goals`, `thread_dynamic_tools`, `thread_spawn_edges`, `agent_jobs`, `agent_job_items`. | `turn_context` records cwd/date/timezone/policies/model/effort/summary/user instructions/truncation policy; `compacted` records replacement history. | Strong example of per-turn context records plus relational thread/job metadata. |
| Claude | Per-project JSONL with user/assistant/system entries, queue operations, attachments, file-history snapshots. | No separate todo/plan DB observed in local store. | Assistant messages include usage/cache fields and a `context_management` field, often null in sampled sessions. | Transcript-first; useful for raw audit, weaker for resumable task state. |
| Kiro | Session JSONL with `Prompt`, `AssistantMessage`, `ToolResults`; content blocks include text/toolUse/toolResult. | Session JSON has `session_state`: conversation metadata, model/runtime state, permissions, agent name. | Stores context usage percentage in runtime model state. | Clean split between event log and compact session metadata, but less task-specific projection observed. |

Implication for astra:

- Adopt Copilot/Codex's split between raw event log and indexed state.
- Keep Codex-style per-turn context records, but make them more explicit as
  `context_manifests`.
- Keep Copilot-style plan/todo/checkpoint state as first-class projection.
- Do not rely on Claude/Kiro-style transcript-only resume for long web sessions.
- Add retrieval indexes because none of the local stores alone solves the
  "10GB session, find one old detail" web-agent case.

## Core Model

The central rule:

> Store complete state. Send a curated context manifest.

There are three different representations:

1. **Audit state**: complete append-only facts for replay, debugging, and
   compliance. Examples: `agent_events`, `conversation_log`, raw tool output
   artifacts, checkpoints.
2. **Current projection**: compact mutable/read-optimized state for UI and
   context assembly. Examples: active plan, todos, recent files, blocked tools,
   skill candidates, current run status.
3. **Runtime context manifest**: the exact per-turn slice sent to the LLM, with
   token budgets, provenance, dropped candidates, hashes, and reasons.

Never use one table or JSON blob for all three.

## Session State Management Layer

Yes: astra should add a dedicated session-state management layer that supports
both local CLI state and remote web-agent state. Without this layer, CLI and web
will gradually diverge, and context management will become impossible to reason
about.

The layer should sit between agent execution and physical storage:

```text
Agent loop / Web UI / CLI
        │
        ▼
SessionStateService
  - load display projection
  - append audit events
  - read/write CSL
  - update current projections
  - load context candidates
  - save context manifest
  - manage plans/todos/skills/artifacts
        │
        ├── LocalSessionStateBackend
        │     - local CLI files / SQLite / journal
        │     - works offline
        │     - can sync later
        │
        ├── RemoteSessionStateBackend
        │     - MatrixOne via astra API server
        │     - cloud authoritative for web agent
        │     - cross-device resume
        │
        └── HybridSessionStateBackend
              - local-first CLI with cloud sync
              - conflict detection and merge policy
```

This is not only a repository abstraction. It needs to expose the semantic
operations the agent runtime cares about. If the interface is just `get_json`
and `put_json`, context management will leak back into callers.

### Required Interface Shape

The first version can be expressed as a Rust trait and mirrored by HTTP APIs for
remote access:

```rust
#[async_trait]
pub trait SessionStateService: Send + Sync {
    async fn begin_turn(&self, input: BeginTurnInput) -> Result<TurnHandle, SessionStateError>;
    async fn load_display_state(&self, session_id: &str) -> Result<DisplaySessionState, SessionStateError>;
    async fn load_transcript_page(&self, query: TranscriptQuery) -> Result<TranscriptPage, SessionStateError>;
    async fn search_session_history(&self, query: SessionHistorySearch) -> Result<SessionHistoryMatches, SessionStateError>;
    async fn load_session_history_window(&self, query: SessionHistoryWindow) -> Result<TranscriptPage, SessionStateError>;
    async fn load_context_candidates(&self, input: ContextCandidateInput) -> Result<Vec<ContextCandidate>, SessionStateError>;
    async fn save_context_manifest(&self, manifest: ContextManifest) -> Result<(), SessionStateError>;
    async fn append_audit_event(&self, event: AgentEventInput) -> Result<String, SessionStateError>;
    async fn append_csl_entry(&self, entry: ConversationLogEntry) -> Result<(), SessionStateError>;
    async fn update_projection(&self, mutation: ProjectionMutation) -> Result<(), SessionStateError>;
    async fn commit_turn(&self, handle: TurnHandle, outcome: TurnOutcome) -> Result<(), SessionStateError>;
}
```

Key rule: `ContextAssembler` consumes `load_context_candidates()` and writes
`save_context_manifest()`. It should not know whether candidates came from local
files, SQLite, MatrixOne, or a remote HTTP API.

Second key rule: display transcript rows are not an automatic runtime fallback
for prompt reconstruction. If the LLM needs old details that are outside the
CSL/recent-tail/context manifest, it must use explicit read-only history tools:
`session_history_page`, `session_history_search`, and
`session_history_around`. Those tools are current-session scoped, token-bounded,
and must validate `user_id` after loading rows from MatrixOne.

### Backend Modes

| Mode | Authority | User experience | Storage |
| --- | --- | --- | --- |
| CLI local-only | Local machine | Works offline; no cross-device resume | Existing local session state, local journal/SQLite |
| CLI local-first + sync | Local first, cloud mirror | Works offline and can resume remotely after sync | Local backend + `StateSyncService` + MatrixOne |
| Web agent | Cloud | Cross-device, browser-only, server-managed tools/workspace | MatrixOne authoritative |
| Edge-assisted web | Cloud for state, edge for workspace/tool authority | Web UI can control local workspace through edge | MatrixOne + edge bridge state |

This also clarifies ownership:

- CLI should not directly depend on MatrixOne schema for normal operation.
- Web agent should not depend on browser-local transcript state.
- Agent loop should not care whether the state is local or remote.
- Sync is a backend concern, not a prompt-construction concern.

### Why This Layer Matters

1. **Context correctness**: one code path decides what state is eligible for LLM
   context, regardless of CLI or web.
2. **Token savings**: token budgets and context manifests are shared, so CLI and
   web benefit from the same pruning/compaction policy.
3. **Cross-device resume**: web uses remote backend directly; CLI can push/pull
   the same semantic state.
4. **Extensibility**: todos, plans, skills, artifacts, delegation, approvals,
   and future state categories become projection mutations rather than one-off
   table writes.
5. **Testing**: the same contract can run against local in-memory, local SQLite,
   and MatrixOne backends.

### Recommended Boundary

Do add:

- `SessionStateService`: semantic state operations.
- `SessionStateBackend`: physical persistence adapter.
- `ContextAssembler`: builds runtime context manifest from candidates.
- `SessionDisplayProjector`: builds UI/CLI display state from durable state.
- `SessionSyncAdapter`: syncs local and remote backends when applicable.

Do not add:

- A generic JSON KV layer as the main interface.
- Direct MatrixOne calls from UI or agent loop code.
- Separate CLI-only and web-only context assemblers.
- Browser-owned session state as a source of truth.

## Cost-Optimized Web Agent Data Flow

The reference stores suggest a practical rule for astra web agent:

> Keep the authoritative state remote and durable, but make the hot path read
> small projections and deltas. Full raw history exists for audit/retrieval, not
> for every session open or every turn.

### Hot / Warm / Cold State

| Tier | Data | Read frequency | Write path |
| --- | --- | --- | --- |
| Hot | `agent_sessions`, active `agent_runs`, `session_state_items`, `session_todos`, latest `session_transcript_items`, latest run events | Every Web open, every turn | Sync in request path |
| Warm | `session_history_chunks`, `session_tool_outputs`, recent `conversation_log` snapshots/deltas, recent context manifests | Resume, search, scroll, debug | Mostly async, bounded sync refs |
| Cold | Raw long transcript payloads, full tool outputs, large artifacts, old audit events | Explicit open/export/retrieval | Compressed append-only storage |

Hot tables must contain enough information for the UI and agent to resume
without scanning raw logs. Cold data must remain addressable by stable refs and
hashes.

### Web Client Cache

The browser should cache session display state to reduce MatrixOne and network
cost, but it must never become the source of truth.

Session identity is also remote-owned. A Web chat URL must use the MatrixOne
`agent_sessions.session_id` as its only chat identifier. Creating a new Web chat
therefore starts with `POST /sessions`; the returned `session_id` becomes the
route id, cache key, transcript key, archive/delete target, and stream
`session_id`. Browser-generated ids are allowed only for unsent draft composer
state and must not enter `/chats/{id}` routes or durable session APIs.

Use IndexedDB or an equivalent browser store for:

- recent `session_state` projection;
- transcript pages keyed by `(session_id, before_seq, limit)` or page boundary;
- active run event tail keyed by `(run_id, last_event_idx)`;
- latest context manifest summary for the context side panel;
- artifact previews and metadata, not large raw payloads.

Every server response should include revision/watermark fields:

```json
{
  "session_id": "s_...",
  "state_revision": {
    "monotonic_id": 42,
    "revision_hash": "sha256:...",
    "device_fingerprint": "fp_..."
  },
  "transcript_high_watermark": 9182,
  "run_event_high_watermark": 331,
  "manifest_id": "ctx_...",
  "page_hash": "sha256:...",
  "replay_required": false,
  "transcript_replay_required": false,
  "run_event_replay_required": false
}
```

Open-session flow:

```text
Web has cached:
  state_revision=40
  transcript_high_watermark=9000
  run_event_high_watermark=300
  ↓
GET /sessions/{id}/state?known_state_revision=40
GET /sessions/{id}/transcript?after_seq=9000
GET /chat/runs/{run_id}/events?after_idx=300
  ↓
Server returns only deltas or 304/not_modified equivalents
```

Scroll-up flow:

```text
1. Browser checks cached transcript page.
2. If present, render immediately.
3. Background revalidate with page boundary + page_hash.
4. If changed, replace page and update watermark.
```

SSE/WS stream flow:

- User message is shown optimistically in the browser.
- The web client sends an explicit `context.thinking` request hint with the
  turn, and the server resolves it into the run's `ThinkingConfig` before the
  first LLM call. Provider SSE deltas are forwarded while they arrive instead
  of waiting for aggregate collection.
- While an assistant turn is streaming, the browser shows a Thinking placeholder.
  Provider-emitted `reasoning_delta`, `thinking_delta`, and model-emitted
  `<think>` / `<thinking>` text are normalized into a bounded, collapsible
  assistant-local Thinking timeline so live reasoning does not displace the
  main transcript. If the provider/model does not expose reasoning content, the
  placeholder is removed after completion rather than persisted as fake detail.
  The open timeline is capped to a small scroll region; long reasoning blocks
  are clamped with an explicit "Show more" affordance, and completed turns
  collapse to a short Thinking summary / Done row.
- Server assigns canonical `item_seq`, `run_id`, and event indexes.
- Stream events update IndexedDB as canonical rows arrive.
- On reconnect, browser sends last seen run event index and transcript
  watermark.

This is the Web analogue of local CLI session files, but remote MatrixOne remains
authoritative.

<!-- GAP-FIX: G24 -->

#### Cold-Start Hydration

Warm cache and cold-start cache use different contracts. A browser with no
IndexedDB rows for a session must not treat the server's high watermarks as
locally applied watermarks.

Cold-start request:

```text
GET /sessions/{id}/state?known_state_revision=0&client_cache_empty=true
```

If the server has any transcript rows or active run events, `/sessions/{id}/state`
returns:

```json
{
  "session_id": "s_...",
  "state_revision": {"monotonic_id": 42, "revision_hash": "sha256:..."},
  "transcript_high_watermark": 9182,
  "active_run": {
    "run_id": "run_...",
    "run_event_high_watermark": 331,
    "replay_required": true,
    "replay_start_event_idx": 0
  },
  "replay_required": true,
  "transcript_replay_required": true,
  "run_event_replay_required": true
}
```

Client rules:

1. Hydrate transcript pages from the transcript API before marking the timeline
   complete. It may page from `after_seq=0` or request the latest page first and
   continue backwards, but it must keep `transcript_replay_required=true` until
   the visible range is backed by durable rows.
2. For active run events, reconnect from the beginning: use
   `/chat/runs/{run_id}/stream?last_index=-1`, or
   `from_index=0&inclusive=true` if the implementation rejects negative
   indexes. A cold client must never call `stream?last_index=<server hwm>`.
3. Only after IndexedDB commits the replayed event rows and transcript pages may
   it advance local `run_event_high_watermark` / `transcript_high_watermark`.
4. Warm clients keep using delta APIs with their local applied watermarks.

Server rules:

- `known_state_revision=0` or `client_cache_empty=true` forces replay flags when
  server high watermarks are non-zero.
- The state API returns hot projection for fast UI paint, but projection is not
  proof that transcript/run-event rows are present locally.
- Replay flags are advisory for UI loading state and mandatory for SDK cache
  correctness.

<!-- /GAP-FIX: G24 -->

<!-- GAP-FIX: G13 -->

#### Revision Reconciliation and Device Lease

`state_revision` has two parts:

- `monotonic_id`: server-owned increasing integer used for `if-none-match`
  comparison and delta ranges.
- `revision_hash`: hash over `(session_id, monotonic_id, device_fingerprint,
  transcript_high_watermark, run_event_high_watermark, state_projection_hash)`.

The server compares `known_state_revision.monotonic_id`, not the full hash, to
decide whether a delta can be returned. The full hash detects device-specific
rollback, stale workspace state, or corrupted local cache.

```sql
CREATE TABLE IF NOT EXISTS session_device_leases (
  lease_id            VARCHAR(128) PRIMARY KEY,
  user_id             VARCHAR(128) NOT NULL,
  session_id          VARCHAR(128) NOT NULL,
  device_id           VARCHAR(128) NOT NULL,
  device_fingerprint  VARCHAR(128) NOT NULL,
  trust_level         VARCHAR(32) NOT NULL DEFAULT 'new_device',
  status              VARCHAR(32) NOT NULL DEFAULT 'active',
  last_monotonic_id   BIGINT NOT NULL DEFAULT 0,
  expires_at          TIMESTAMP NOT NULL,
  revoked_at          TIMESTAMP NULL,
  request_id          VARCHAR(128) NULL,
  trace_id            VARCHAR(128) NULL,
  created_at          TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at          TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  UNIQUE KEY uq_session_device (session_id, device_id),
  INDEX idx_device_leases_user_session (user_id, session_id, status, updated_at),
  INDEX idx_device_leases_fingerprint (user_id, device_fingerprint, status),
  INDEX idx_device_leases_expiry (status, expires_at)
);
```

Reconciliation paths:

- **delta**: known monotonic id is within the server retention window; return
  deltas plus new revision hash.
- **gap full reset**: known monotonic id is too old or any page hash mismatches;
  return full hot projection and reset local cache watermarks.
- **CAS conflict**: client attempts to write based on stale monotonic id; return
  `409` with the current revision and no state mutation.

`trust_level` values:

- `trusted`: known device, normal delta behavior.
- `new_device`: first use; full hot projection is allowed after auth but write
  actions may require step-up confirmation.
- `unknown_device`: suspicious or revoked; reads are denied or restricted by
  product policy.

Minimum API additions:

- `POST /sessions/{id}/device/revoke`
- `GET /sessions/{id}/devices`

<!-- GAP-FIX: G25 -->

#### Device Lease End Event Parity

Explicit revoke and passive expiry are security-equivalent from the browser's
point of view. Both paths must produce a durable lease event and a user-scoped
push notification so untrusted devices clear local state without waiting for a
future failed request.

Additive audit table:

```sql
CREATE TABLE IF NOT EXISTS session_device_lease_events (
  lease_event_id     VARCHAR(128) PRIMARY KEY,
  lease_id           VARCHAR(128) NOT NULL,
  user_id            VARCHAR(128) NOT NULL,
  session_id         VARCHAR(128) NOT NULL,
  device_id          VARCHAR(128) NOT NULL,
  device_fingerprint VARCHAR(128) NOT NULL,
  event_type         VARCHAR(64) NOT NULL,
  reason             VARCHAR(64) NOT NULL,
  ended_at_server    TIMESTAMP NOT NULL,
  request_id         VARCHAR(128) NULL,
  trace_id           VARCHAR(128) NULL,
  created_at         TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  INDEX idx_lease_events_user_created (user_id, created_at),
  INDEX idx_lease_events_session_device (session_id, device_id, created_at)
);
```

Lease states:

```text
active -> revoked
active -> expired
revoked/expired -> terminal
```

Event parity:

| State transition | SSE/WS event | Required payload |
| --- | --- | --- |
| explicit revoke | `device_revoked` | `lease_id`, `session_id`, `device_id`, `device_fingerprint`, `reason`, `ended_at_server` |
| passive expiry | `device_lease_expired` | same fields as `device_revoked` |

Implementations may also emit the normalized event
`device_lease_ended {event_type, reason, ...}` for SDK consumers, but they must
keep the explicit event names for audit queries.

Expiry can be detected by a background scanner or by read-time lease checks. The
first detector owns the transition with a compare-and-set update:

```sql
UPDATE session_device_leases
SET status='expired', revoked_at=CURRENT_TIMESTAMP, updated_at=CURRENT_TIMESTAMP
WHERE lease_id=? AND status='active' AND expires_at <= CURRENT_TIMESTAMP;
```

If the update affects one row, the server inserts `session_device_lease_events`
and broadcasts `device_lease_expired`. If another worker already ended the
lease, the detector must not emit a duplicate.

Client SDK behavior for either `device_revoked` or `device_lease_expired`:

- close active streams for that session/device;
- clear IndexedDB rows, localStorage, sessionStorage, and memory caches scoped to
  the session;
- render a signed-out or re-auth required state before any further API call;
- never reuse optimistic messages created after `ended_at_server`.

<!-- /GAP-FIX: G25 -->

<!-- /GAP-FIX: G13 -->

<!-- GAP-FIX: G19 -->

#### Web Event Watermark Atomicity

IndexedDB event application is transactional. For each run stream batch, the
browser writes event rows and advances `run_event_high_watermark` in the same
IndexedDB transaction. It must never update the watermark before all events up
to that index are durable in the local cache.

Client-side tables:

```text
run_events(run_id, event_idx, event_hash, payload, applied_at)
session_watermarks(session_id, transcript_high_watermark, run_event_high_watermark, state_monotonic_id)
```

Apply protocol:

```text
begin transaction
  for event in batch ordered by event_idx:
    if event_idx <= current_watermark and event_hash matches: skip
    if event_idx != current_watermark + 1: abort with gap_detected
    insert run_events row
    update current_watermark = event_idx
  update session_watermarks.run_event_high_watermark = current_watermark
commit
```

Gap recovery:

- If the client observes `event_idx > last_ok_idx + 1`, it aborts the batch,
  clears non-applied rows for that run, and reconnects with
  `last_index=last_ok_idx`.
- If replay still contains a gap, it resets the run-event cache for that run and
  replays from `last_index=-1`.
- Transcript and run-event watermarks are reconciled independently; a transcript
  cache gap does not advance run-event state.

Multi-tab coordination:

- Tabs share watermarks through `BroadcastChannel` when available.
- A `SharedWorker` may own the single SSE connection, but it is an optimization,
  not a correctness requirement.
- There is no primary-tab lock. Each tab applies events idempotently by
  `(run_id, event_idx, event_hash)` and ignores stale broadcasts.

This closes G15 r2: run replay correctness requires server event ordering and
client watermark atomicity.

<!-- /GAP-FIX: G19 -->

### Cheap Context Construction

`context_manifest` construction should be deterministic and staged. Do not start
with vector search or LLM extraction.

Default turn flow:

```text
1. Load hot projection:
   - session anchor
   - active/paused plan
   - active/pending todos
   - active run/wait state
   - workspace/tool/skill state
   - recent transcript tail

2. Decide whether retrieval is needed:
   - explicit history request
   - continuation/resume ambiguity
   - missing referenced artifact/file/todo
   - user refers to old time, old error, old decision, or "that thing"

3. If needed, query history in layers:
   a. structured filters over `session_history_chunks`
   b. full-text search over preview/index text
   c. vector search only if a/b are insufficient

4. Render within fixed token caps.

5. Persist manifest header + item refs.
```

Token cost should be capped by zone, for example:

| Zone | Default cap |
| --- | --- |
| Session anchor | 300-600 tokens |
| Active/paused plan | 500-1000 tokens |
| Todos | 300-800 tokens |
| Recent tail | 1000-2000 tokens |
| Retrieved old chunks | 1000-4000 tokens |
| Tool/artifact previews | 500-1500 tokens |

Completed or unrelated old todos should not enter the prompt. They stay in the
database and retrieval index.

### Manifest Storage Cost

Do not store a huge rendered prompt body by default.

Persist:

- manifest header;
- item refs;
- token estimates;
- source hashes;
- inclusion/dropping reasons;
- small rendered excerpts when they are needed for audit.

Optional full rendered prompt capture should be controlled by debug/audit mode
or short retention. This keeps `context_manifests` useful without turning it
into another copy of the transcript.

### Lazy Indexing

Synchronous request path should write only what is needed for correctness:

- append raw event/log rows;
- update hot projections;
- append transcript display items;
- append run events;
- write artifact refs/previews.

The following should be async or batched:

- generating `session_history_chunks` for old or large payloads;
- full-text index refresh;
- vector embedding;
- deep next-action extraction from natural language;
- backfilling transcript projection for old sessions;
- old context manifest rebuild.

This mirrors the useful part of Copilot/Codex local designs: immediate append
plus lightweight indexes first, heavier searchable state later.

### Next-Action Extraction Cost

For "continue" semantics, do not run an LLM extractor on every assistant
message.

Priority order:

1. Direct structured events: plan/todo tool, task status event, structured
   output, run status.
2. Cheap rule extraction: detect explicit "next steps", "remaining", "todo",
   "blocked", "follow-up" sections.
3. Small-model extraction only when the response contains plausible next-action
   text and no structured state exists.
4. Low-confidence extraction becomes `suggested_next_action`, not an
   automatically executable todo.

Suggested next actions should have `status` and `expires_at`. If a user says
"continue" days later and many suggestions exist, the context builder should
prefer the newest accepted/active suggestion and may ask for clarification.

<!-- GAP-FIX: G12 -->

#### Next-Action Confidence State Machine

Next-action extraction can produce multiple suggestions in one turn. Suggestions
from different sources coexist; lower-confidence candidates must not overwrite a
structured high-confidence continuation.

Thresholds:

| Confidence | Action |
| --- | --- |
| `>=0.8` | Auto-accept and state the basis in the assistant response. |
| `0.5-0.8` | Write candidates and ask the user to choose. |
| `<0.5` | Do not guess; respond that the reference is unclear and ask for a more specific target. |

State machine:

```text
structured_event hit
  -> suggestion(status=accepted, source=structured_event, confidence=1.0)
rule hit ambiguous
  -> suggestion(status=pending, source=rule, confidence=0.5..0.8)
small_model hit low confidence
  -> suggestion(status=pending, source=small_model, confidence=<0.8)
user chooses candidate
  -> apply_suggestion event references suggested_next_action.id
```

A single turn may create at most `5` suggestions. Candidates have stable ids,
source, confidence, provenance refs, and expiry:

- `approval`: 24h
- `todo`: 7d
- `hint`: 1h

Ask-user fatigue policy: if the same session triggers 3 clarification prompts
within 1 hour, lower the auto-accept threshold by one band only for structured
or rule-backed candidates with explicit provenance. Small-model-only candidates
still require user confirmation.

<!-- /GAP-FIX: G12 -->

### Normal Query Budget

Opening a session should be roughly:

1. one indexed query for session metadata and hot projection;
2. one indexed query for latest transcript page;
3. one indexed query for active run/status/events;
4. optional small query for latest context manifest summary.

Continuing a paused task should be roughly:

1. hot projection query;
2. active/paused run query;
3. active todo/plan query;
4. recent transcript tail query;
5. optional retrieval top-K query.

The design should treat anything beyond that as either cache miss, explicit
search, artifact expansion, or background work.

## Compatibility With Local CLI Session State

This web-agent design must not become a separate state model from the local CLI.
The intended relationship is:

> Shared session-state semantics, independent physical backends.

### Compatibility Requirements

- Existing CLI local session management is not removed by this design.
- CLI can continue using local files, JSONL, SQLite, journals, or the current
  local session workspace format.
- Web agent uses MatrixOne as the authoritative remote backend.
- Hybrid CLI mode can remain local-first and sync to MatrixOne when the user is
  authenticated and online.
- `ContextAssembler` must depend only on `SessionStateService` contracts. It
  must not directly read local session files or MatrixOne tables.
- Web UI must depend only on display/session APIs. It must not know whether a
  session originated from CLI, Web, or edge-assisted execution.
- State categories must be portable across backends: `anchor`, `plan_state`,
  `todo_state`, `tool_ref`, `artifact_ref`, `workspace_state`, `summary`,
  `suggested_next_action`, `run_state`, and `context_manifest`.

### Backend Responsibilities

| Responsibility | Local CLI backend | Remote Web backend | Hybrid backend |
| --- | --- | --- | --- |
| Authoritative state | Local machine | MatrixOne | Local while offline, MatrixOne after sync |
| Raw history | Local JSONL/journal | `conversation_log` / audit tables | Both, with sync watermarks |
| Display transcript | Local projection or materialized view | `session_transcript_items` | Local cache + remote projection |
| Plan/todo state | Local SQLite/JSON/projection | `session_todos`, `plans`, `session_state_items` | Merge by version/status |
| Context manifest | Local manifest log | `context_manifests` | Sync manifest refs and summaries |
| Artifact storage | Local files | artifact refs/object storage/MatrixOne metadata | Local refs plus uploaded refs |

### Portable State Envelope

Backends may store data differently, but sync and context assembly should use a
portable envelope:

```json
{
  "session_id": "s_...",
  "user_id": "u_...",
  "state_revision": 42,
  "category": "todo_state",
  "item_key": "todo:abc",
  "status": "active",
  "version": 7,
  "source": "agent",
  "provenance_ref": {
    "backend": "local|remote",
    "event_id": "evt_...",
    "run_id": "run_..."
  },
  "payload": {},
  "summary_text": "...",
  "token_estimate": 120,
  "updated_at": "..."
}
```

This envelope is the boundary between storage and context logic. Local and
remote backends can map it to different tables/files, but the agent should see
the same semantic object.

### Migration Strategy

1. Define `SessionStateService` and in-memory test backend.
2. Implement `RemoteSessionStateBackend` for new Web sessions.
3. Add adapter for current CLI local session data.
4. Move `ContextAssembler` to consume only the service contract.
5. Add hybrid sync for selected categories: transcript metadata, plan/todo,
   artifacts, summaries, context manifest metadata.
6. Backfill older CLI sessions lazily when opened or explicitly synced.

### Non-Goals

- Do not force all CLI state into MatrixOne before Web agent can ship.
- Do not make Web UI read local CLI files.
- Do not require local CLI to store state in the same schema as MatrixOne.
- Do not duplicate context assembly logic per backend.

## Shared LLM Interaction Layer

CLI and Web should not have separate LLM interaction logic. They should share
the same policy and rendering pipeline, while swapping only transport, storage,
UI, and tool execution adapters.

Shared components:

- `ContextAssembler`: loads candidates from `SessionStateService`, applies
  budgets, and writes `context_manifest`.
- `PromptRenderer`: renders a manifest into model-specific chat/messages input.
- `ToolSchemaSelector`: chooses tool schemas based on agent, workspace,
  permissions, skills, and model capability.
- `RetrievalPolicy`: decides when to retrieve old history and which retrieval
  tiers to use.
- `CompactionPolicy`: decides when to compact and which structured state must be
  preserved.
- `PostTurnExtractor`: updates projections from structured events and cheap
  extraction after a turn.
- `LLMClient` abstraction: handles model call, streaming, token usage, retryable
  errors, and provider-specific protocol differences.

Different adapters:

| Concern | CLI | Web agent |
| --- | --- | --- |
| User interaction | Terminal/TUI | Browser UI |
| Streaming transport | stdout/TUI events | SSE/WS |
| State backend | Local or hybrid backend | Remote MatrixOne backend |
| Tool execution | Local filesystem/shell/MCP | Cloud tools or edge bridge |
| Browser cache | None or local UI cache | IndexedDB display cache |
| Auth | Local config/token | Web auth/session cookies |

Authentication is also shared at the service boundary. CLI and Web both call
the same runtime `/auth/register`, `/auth/login`, `/auth/refresh`,
`/auth/logout`, and `/auth/me` endpoints backed by `AuthService`. The clients
only differ in credential storage: CLI writes the selected profile in
`~/.astra/credentials.json`, while Web writes httpOnly cookies. Registration is
a token-producing operation for both clients, so neither CLI nor Web should
perform a second login after a successful register response.

Client protocol libraries are implementation details over the same runtime
contract. The contract remains REST + SSE + JSON. CLI uses the Rust
`astra_thin_client`; Web uses a Next.js BFF plus the internal
`web/lib/runtime-client` boundary for API URL resolution, bearer auth, token
refresh, JSON parsing, and runtime error context. The planned JS SDK is the
TypeScript client for this contract, not a replacement for the protocol, and it
must not make CLI depend on JavaScript. Public runtime paths and Web-used wire
response DTOs, including session, transcript, artifact, model, skill, auth, and
chat-run payloads, are owned by `@astra/sdk` and mirrored in the Rust ThinClient
where applicable; the Web app should import those contracts instead of
redeclaring backend payload shapes. Shared HTTP helper behavior such as error
body parsing, header merging, JSON-capable method checks, and JWT subject
extraction also lives in `@astra/sdk`; the Web BFF should keep only
Next-specific cookie/session behavior locally. Web server routes should prefer
SDK high-level methods for stable operations such as raw session
create/read/list/update, transcript pagination, artifact listing, model listing,
skill catalog listing, and non-streaming chat run creation. Direct raw response handling remains
appropriate where the BFF must proxy an open SSE stream.

Shared LLM turn flow:

```text
User input
  ↓
SessionStateService.begin_turn()
  ↓
ContextAssembler.load_context_candidates()
  ↓
ContextAssembler.save_context_manifest()
  ↓
PromptRenderer.render(manifest)
  ↓
LLMClient.stream()
  ↓
Tool execution adapter handles tool calls
  ↓
PostTurnExtractor updates projections
  ↓
SessionStateService.commit_turn()
```

Rules:

- CLI and Web must produce equivalent context manifests for equivalent session
  state, modulo backend-specific refs and available tools.
- Prompt rendering must be deterministic for a given manifest, model, and tool
  set.
- Tool execution may differ, but tool results must return to the same semantic
  event/projection model.
- Provider-specific prompt/cache behavior belongs in shared `PromptRenderer` and
  `LLMClient`, not in CLI/Web UI code.
- If Web adds a context-saving policy, CLI should inherit it automatically unless
  explicitly disabled by execution profile.

### State Layers

| Layer | Purpose | Primary storage |
| --- | --- | --- |
| L0 session anchor | Goal, active constraints, current phase, must-remember facts | `session_state_items` current projection |
| L1a structured facts | Active files, plan/todo state, errors, decisions, tool refs | `session_state_items`, `plans`, `agent_tasks`, `session_artifacts` |
| L1b narrative summary | Human-readable compact summary of older history | `conversation_log` snapshots/deltas and `session_state_items` summary rows |
| L2 audit transcript | Full conversation/event history | `conversation_log`, `agent_events`, `agent_event_edges` |
| L3 durable memory | Cross-session preferences, lessons, personal skills | `user_preferences`, learning snapshots, skills tables, future memory tables |
| Runtime prompt | Budgeted per-turn manifest | `context_manifests`, `context_manifest_items` |

## Proposed Schema

This is additive to the existing schema. Existing `agent_sessions`,
`agent_events`, `ctx_snapshots`, `conversation_log`, skills, plans, and tasks
remain useful.

MatrixOne/MySQL note: fields used in `WHERE`, `JOIN`, `ORDER BY`, or pagination
must be real columns. JSON payloads should be `LONGTEXT` or JSON-compatible text
for storage, but production queries must not filter inside them.

### 1. Durable Runs

This implements the missing `DatabaseRunStateStore` behind the existing
`RunStateStore` trait.

<!-- GAP-FIX: G15 -->

```sql
CREATE TABLE IF NOT EXISTS agent_runs (
  run_id                  VARCHAR(128) PRIMARY KEY,
  user_id                 VARCHAR(128) NOT NULL,
  session_id              VARCHAR(128) NOT NULL,
  parent_run_id           VARCHAR(128) NULL,
  root_run_id             VARCHAR(128) NULL,
  ancestor_path           TEXT NULL,
  depth                   INT NOT NULL DEFAULT 0,
  delegation_id           VARCHAR(128) NULL,
  agent_id                VARCHAR(128) NULL,
  retry_of                VARCHAR(128) NULL,
  status                  VARCHAR(32) NOT NULL,
  execution_mode          VARCHAR(32) NOT NULL DEFAULT 'cloud',
  trigger_type            VARCHAR(64) NOT NULL DEFAULT 'user_message',
  trigger_event_id        VARCHAR(128) NULL,
  waiting_for             VARCHAR(255) NULL,
  owner_pod_id            VARCHAR(128) NULL,
  owner_lease_expires_at  TIMESTAMP NULL,
  run_generation          BIGINT NOT NULL DEFAULT 1,
  last_event_idx          INT NOT NULL DEFAULT -1,
  checkpoint_version      VARCHAR(32) NULL,
  checkpoint_json         LONGTEXT NULL,
  error_code              VARCHAR(128) NULL,
  error_message           TEXT NULL,
  retry_count             INT NOT NULL DEFAULT 0,
  retry_scope             VARCHAR(32) NOT NULL DEFAULT 'node',
  total_prompt_tokens     BIGINT NOT NULL DEFAULT 0,
  total_completion_tokens BIGINT NOT NULL DEFAULT 0,
  total_tool_calls        BIGINT NOT NULL DEFAULT 0,
  request_id              VARCHAR(128) NULL,
  trace_id                VARCHAR(128) NULL,
  created_at              TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at              TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  INDEX idx_runs_user_updated (user_id, updated_at, run_id),
  INDEX idx_runs_session_updated (session_id, updated_at, run_id),
  INDEX idx_runs_status_waiting (status, waiting_for, updated_at),
  INDEX idx_runs_parent (parent_run_id),
  INDEX idx_runs_root (root_run_id, updated_at),
  INDEX idx_runs_owner (owner_pod_id, owner_lease_expires_at),
  INDEX idx_runs_delegation (delegation_id)
);

CREATE TABLE IF NOT EXISTS run_counters (
  run_id                 VARCHAR(128) PRIMARY KEY,
  next_event_idx         INT NOT NULL DEFAULT 0,
  owner_pod_id           VARCHAR(128) NULL,
  owner_lease_expires_at TIMESTAMP NULL,
  run_generation         BIGINT NOT NULL DEFAULT 1,
  request_id             VARCHAR(128) NULL,
  trace_id               VARCHAR(128) NULL,
  created_at             TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at             TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  INDEX idx_run_counters_owner (owner_pod_id, owner_lease_expires_at)
);

CREATE TABLE IF NOT EXISTS agent_run_events (
  id              BIGINT AUTO_INCREMENT PRIMARY KEY,
  run_id          VARCHAR(128) NOT NULL,
  event_idx       INT NOT NULL,
  user_id         VARCHAR(128) NOT NULL,
  session_id      VARCHAR(128) NOT NULL,
  event_type      VARCHAR(96) NOT NULL,
  event_id        VARCHAR(128) NULL,
  agent_id        VARCHAR(128) NULL,
  idempotency_key VARCHAR(255) NULL,
  event_hash      VARCHAR(128) NULL,
  producer_pod_id VARCHAR(128) NULL,
  payload_json    LONGTEXT NOT NULL,
  request_id      VARCHAR(128) NULL,
  trace_id        VARCHAR(128) NULL,
  created_at      TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  UNIQUE KEY uq_run_event_idx (run_id, event_idx),
  UNIQUE KEY uq_run_event_idempotency (run_id, idempotency_key),
  INDEX idx_run_events_session_created (session_id, created_at, id),
  INDEX idx_run_events_user_created (user_id, created_at, id),
  INDEX idx_run_events_type_created (event_type, created_at)
);
```

Why separate `agent_run_events` from `agent_runs.events`:

- SSE replay after reconnect can page by `(run_id, event_idx)`.
- Load-balanced workers can stream from DB.
- Events can be retained/archived independently from the hot run row.
- No JSON filtering is needed for run list/status queries.

#### Run Event Ordering and Ownership

`agent_run_events.event_idx` is allocated by `run_counters`, not by scanning
`MAX(event_idx)`. The writer opens a DB transaction, locks the `run_counters`
row for the run, verifies the current owner lease, uses `next_event_idx`,
increments it, inserts the event, and updates `agent_runs.last_event_idx`.
`UNIQUE (run_id, event_idx)` is only the final integrity guard.

Only one server pod owns a non-terminal run at a time:

- `agent_runs.owner_pod_id` and `owner_lease_expires_at` identify the current
  writer.
- Normal rolling shutdown writes a `checkpoint_v1`, sets
  `checkpoint_json.graceful=true`, releases or expires the lease, and the next
  pod resumes the run with event `run_resumed_after_restart`.
- Crash recovery without a graceful checkpoint must not silently continue the
  same execution. The run becomes `failed` with a retry suggestion, or a new run
  is created with `retry_of=<old_run_id>`.
- Lease takeover increments `run_generation`. Events carry
  `producer_pod_id`; consumers can detect mixed-generation streams during
  debugging.

`checkpoint_json` uses this v1 shape:

```json
{
  "version": "checkpoint_v1",
  "graceful": true,
  "last_batch_id": "batch_...",
  "extra": {
    "partial_progress": {
      "step_index": 1,
      "total_steps": 3,
      "resumable_marker": "after_step_1"
    }
  }
}
```

`extra.partial_progress` is optional, but if present all three fields are
required. `step_index` and `total_steps` identify the coarse-grained resumable
unit; `resumable_marker` is an opaque executor marker that lets the next owner
resume without replaying already committed tool-output batches.

`POST /chat/runs/{run_id}/input` requires an `idempotency_key`. The server
deduplicates by `(run_id, idempotency_key)` and stores the same key on the
resulting run event. Approval inputs also dedupe by the semantic tuple
`(approval_id, decision, actor_user_id)` so browser retry cannot double-apply an
approval.

Additional first-class run events:

- `approval_expired`
- `approval_retracted`
- `approval_request`
- `approval_decision`
- `approval_condition_modified`
- `requester_confirm`
- `notification_dispatched`
- `notification_acknowledged`
- `edge_timeout`
- `run_resumed_after_restart`

Run event payload contracts:

| event_type | Required payload fields |
| --- | --- |
| `approval_request` | `approval_id`, `required_approvers[]`, `requested_by`, `summary`, `expires_at_server` |
| `approval_decision` | `approval_id`, `approver`, `decision`, `conditions_ref[]`, `decided_at_server` |
| `approval_condition_modified` | `approval_id`, `condition_id`, `operation`, `previous_hash`, `next_hash` |
| `approval_expired` | `approval_id`, `expires_at_server`, `expired_at_server`, `reason` |
| `approval_retracted` | `approval_id`, `retracted_by`, `reason`, `retracted_at_server` |
| `requester_confirm` | `approval_id`, `confirmed_by`, `approval_state_version`, `confirmed_at_server` |
| `notification_dispatched` | `notification_id`, `adapter_name`, `recipient_ref`, `idempotency_key` |
| `notification_acknowledged` | `notification_id`, `adapter_name`, `external_ref`, `acknowledged_at` |
| `edge_timeout` | `edge_bridge_id`, `timeout_ms`, `waiting_for`, `next_status` |
| `run_resumed_after_restart` | `previous_owner_pod_id`, `owner_pod_id`, `run_generation`, `checkpoint_version` |

SSE replay is event-index based. The server sends a heartbeat at least every
15 seconds; the browser treats 45 seconds without any event or heartbeat as a
dead stream and reconnects with its last acknowledged `event_idx`. Multiple
tabs may subscribe to the same run. There is no primary tab; all clients apply
events idempotently by `(run_id, event_idx, event_hash)` and share watermarks
through the web cache.

`status='superseded'` is a terminal audit state for retry replacement. The old
run is never deleted; it remains visible in delegation drill-down and keeps its
artifacts.

<!-- /GAP-FIX: G15 -->

### 2. Session State Projection

This table is the extensibility point for future state categories. It stores
current state items and references back to immutable audit events.

```sql
CREATE TABLE IF NOT EXISTS session_state_items (
  item_id             VARCHAR(128) PRIMARY KEY,
  user_id             VARCHAR(128) NOT NULL,
  session_id          VARCHAR(128) NOT NULL,
  scope               VARCHAR(32) NOT NULL DEFAULT 'session',
  category            VARCHAR(64) NOT NULL,
  item_key            VARCHAR(255) NOT NULL,
  status              VARCHAR(32) NOT NULL DEFAULT 'active',
  priority            INT NOT NULL DEFAULT 0,
  source              VARCHAR(64) NOT NULL,
  provenance_event_id VARCHAR(128) NULL,
  run_id              VARCHAR(128) NULL,
  title               VARCHAR(255) NULL,
  summary_text        TEXT NULL,
  payload_json        LONGTEXT NULL,
  payload_hash        VARCHAR(128) NULL,
  token_estimate      INT NOT NULL DEFAULT 0,
  version             BIGINT NOT NULL DEFAULT 1,
  origin_session_id   VARCHAR(128) NULL,
  origin_chunk_id     VARCHAR(128) NULL,
  origin_state_item_id VARCHAR(128) NULL,
  expires_at          TIMESTAMP NULL,
  request_id          VARCHAR(128) NULL,
  trace_id            VARCHAR(128) NULL,
  created_at          TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at          TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  UNIQUE KEY uq_state_current (session_id, scope, category, item_key),
  INDEX idx_state_session_category (session_id, category, status, priority),
  INDEX idx_state_user_category (user_id, category, status, updated_at),
  INDEX idx_state_user_scope_category (user_id, scope, category, status, priority),
  INDEX idx_state_origin_session (origin_session_id, category, status),
  INDEX idx_state_expires (expires_at),
  INDEX idx_state_provenance (provenance_event_id)
);

CREATE TABLE IF NOT EXISTS session_state_item_events (
  id                  BIGINT AUTO_INCREMENT PRIMARY KEY,
  item_id             VARCHAR(128) NOT NULL,
  user_id             VARCHAR(128) NOT NULL,
  session_id          VARCHAR(128) NOT NULL,
  category            VARCHAR(64) NOT NULL,
  item_key            VARCHAR(255) NOT NULL,
  mutation            VARCHAR(32) NOT NULL,
  previous_hash       VARCHAR(128) NULL,
  next_hash           VARCHAR(128) NULL,
  previous_version    BIGINT NULL,
  next_version        BIGINT NULL,
  payload_json        LONGTEXT NULL,
  provenance_event_id VARCHAR(128) NULL,
  request_id          VARCHAR(128) NULL,
  trace_id            VARCHAR(128) NULL,
  created_at          TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  INDEX idx_state_events_item_created (item_id, created_at, id),
  INDEX idx_state_events_session_created (session_id, created_at, id),
  INDEX idx_state_events_category_created (category, created_at)
);
```

Initial categories:

- `anchor`: current user goal, constraints, active phase.
- `summary`: L1b narrative summary.
- `active_file`: files or symbols recently used.
- `tool_ref`: important tool results or artifact references.
- `error_state`: recent errors, blocked tools, failed commands.
- `decision`: durable decisions and why they were made.
- `finding`: durable investigation findings that should survive compaction.
- `benchmark`: measurements and comparison results.
- `citation`: external or historical references that may need exact recall.
- `plan_state`: active plan pointer and phase.
- `todo_state`: concise todo projection.
- `approval_state`: pending or remembered approvals.
- `skill_hint`: selected or likely relevant skills.
- `active_skill`: per-session selected personal skill version.
- `durable_decision`: user-level decision promoted across sessions.
- `engineering_rule`: user-level engineering preference or invariant.
- `rejected_pattern`: user-level pattern the agent should avoid.
- `workspace_state`: cloud/edge workspace identity, branch, snapshot.
- `delegation_state`: child sessions, subagent work, handoff summary.

This table is not for raw transcript storage. It is the current projection that
context assembly can query cheaply.

<!-- GAP-FIX: G14 -->

`session_state_item_events.mutation` is an enum-like contract:

- `insert`
- `update`
- `replace`
- `archive`
- `delete`
- `bubble_up`
- `apply_suggestion`
- `activate`

`bubble_up` payload contract:

```json
{
  "bubble_seq": 7,
  "severity": "critical",
  "source_run_id": "run_child",
  "original_item_id": "state_finding_...",
  "bubble_target_scope": "root_session",
  "summary": "Critical finding from child reviewer",
  "artifact_refs": ["artifact_..."]
}
```

`apply_suggestion` payload contract:

```json
{
  "suggested_next_action_id": "sna_...",
  "chosen_candidate_id": "cand_a",
  "source": "user_explicit",
  "confidence_at_accept": 1.0,
  "retry_scope": null
}
```

Retry contract:

- `agent_runs.retry_scope` is one of `node`, `subtree`, or `siblings`.
- A retry replacement writes the new run with `retry_of=<old_run_id>` and the
  selected `retry_scope`.
- The replaced run transitions to `status='superseded'`; no run, state item, or
  artifact is physically deleted.
- UI renders old and new branches together; superseded branches are muted but
  drill-down remains available for audit.

Delegation bubble-up:

- Child findings that require parent attention write `bubble_up` on the parent
  session's `delegation_state` item.
- Root-session UI may subscribe only to `bubble_up` state events to render
  global alerts; it does not need to subscribe to every child run.
- G8/G9 preview/artifact counters may be updated by `bubble_up`, but the
  mutation semantics are owned here.

<!-- GAP-FIX: G22 -->

#### Retry Scope Selection and Propagation

`retry_scope` is selected deterministically before a retry run is created. The
server does not ask the LLM to guess the scope after the fact.

Selection order:

1. User explicit wording wins. Phrases equivalent to "only this run" map to
   `node`; "this task and its children" maps to `subtree`; "all groups/all
   siblings" maps to `siblings`.
2. If the accepted `suggested_next_action` already carries `retry_scope`, use
   it after enum validation.
3. If the target run has active child delegations, active blocker descendants,
   or stateful child tool outputs, default to `subtree`.
4. If the target run has no child delegations and its tool effects are
   idempotent or fully superseded by the retry, default to `node`.
5. If the target run is one of several siblings under the same parent and the
   user intent references the group, wave, batch, or "all" peers, use
   `siblings`.
6. If none of the above applies, use `node` and record
   `scope_source='default_node'`.

`apply_suggestion` may carry retry fields when the suggestion represents a
retry:

```json
{
  "suggested_next_action_id": "sna_retry_exec2",
  "chosen_candidate_id": "retry_subtree",
  "source": "structured_event",
  "confidence_at_accept": 0.93,
  "retry_scope": "subtree",
  "target_run_id": "run-L3-exec-2",
  "target_delegation_id": "dl-exec-2",
  "scope_source": "inferred_active_child",
  "retry_reason": "Reviewer child depends on executor output"
}
```

All retry-producing events and commands must carry `retry_scope`:

- `session_state_item_events(mutation='apply_suggestion')`
- retry run creation with `agent_runs.retry_of` and `agent_runs.retry_scope`
- any `agent_run_events` row that represents retry scheduling or retry start
- audit UI links that compare old and superseding branches

Validation failures are hard failures: an unknown retry scope rejects the
suggestion application and writes no retry run.

<!-- /GAP-FIX: G22 -->

Backlog todos are not compaction candidates. `status='backlog'` means the todo
is intentionally kept outside the active prompt but reusable across sessions;
compaction may update its summary refs, but it must not archive or delete it.

<!-- /GAP-FIX: G14 -->

<!-- GAP-FIX: G7 -->

`approval_state` has a structured projection. The compact state item remains the
prompt/display row, while conditions and notification delivery are queryable in
dedicated tables.

```sql
CREATE TABLE IF NOT EXISTS session_approval_conditions (
  condition_id        VARCHAR(128) PRIMARY KEY,
  approval_item_id    VARCHAR(128) NOT NULL,
  user_id             VARCHAR(128) NOT NULL,
  session_id          VARCHAR(128) NOT NULL,
  run_id              VARCHAR(128) NOT NULL,
  condition_type      VARCHAR(64) NOT NULL,
  condition_spec_json LONGTEXT NOT NULL,
  check_trigger       VARCHAR(64) NOT NULL,
  status              VARCHAR(32) NOT NULL DEFAULT 'active',
  added_by            VARCHAR(128) NOT NULL,
  added_at            TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  expires_at_server   TIMESTAMP NULL,
  request_id          VARCHAR(128) NULL,
  trace_id            VARCHAR(128) NULL,
  created_at          TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at          TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  INDEX idx_approval_conditions_item (approval_item_id, status, created_at),
  INDEX idx_approval_conditions_trigger (condition_type, check_trigger, status, expires_at_server),
  INDEX idx_approval_conditions_run (run_id, status)
);

CREATE TABLE IF NOT EXISTS session_external_notifications (
  notification_id     VARCHAR(128) PRIMARY KEY,
  user_id             VARCHAR(128) NOT NULL,
  session_id          VARCHAR(128) NOT NULL,
  run_id              VARCHAR(128) NOT NULL,
  approval_item_id    VARCHAR(128) NULL,
  adapter_name        VARCHAR(64) NOT NULL,
  channel             VARCHAR(64) NOT NULL,
  recipient_ref       VARCHAR(255) NOT NULL,
  status              VARCHAR(32) NOT NULL,
  idempotency_key     VARCHAR(255) NOT NULL,
  external_ref        VARCHAR(255) NULL,
  payload_hash        VARCHAR(128) NOT NULL,
  delivered_at        TIMESTAMP NULL,
  acknowledged_at     TIMESTAMP NULL,
  request_id          VARCHAR(128) NULL,
  trace_id            VARCHAR(128) NULL,
  created_at          TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at          TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  UNIQUE KEY uq_notification_idempotency (adapter_name, idempotency_key),
  INDEX idx_notifications_run_status (run_id, status, created_at),
  INDEX idx_notifications_approval (approval_item_id, status)
);
```

Approval state item payload contract:

```json
{
  "approval_id": "approval:drop-3tables-run123",
  "status": "pending_approvers",
  "required_approvers": ["lead", "risk", "cto"],
  "approvals": [],
  "condition_refs": ["cond_..."],
  "evidence": [{"artifact_ref": "artifact:preprod-report"}],
  "linked_approval_ref": null,
  "expires_at_server": "2026-05-11T03:00:00Z",
  "ttl_seconds": 7200
}
```

State machine:

```text
pending_approvers
  -> approved
  -> pending_requester_confirm
  -> running
  -> completed

pending_approvers -> rejected
approved/pending_requester_confirm -> approval_expired
any pending state -> approval_retracted
```

Approval through an external channel does not directly execute the tool. It
only moves the run to `pending_requester_confirm`; requester confirmation is a
separate `requester_confirm` run event.

External notification adapter contract:

- Runtime emits `notification_dispatched` after inserting a
  `session_external_notifications` row.
- Adapter callbacks emit `notification_acknowledged`.
- Adapter retries use the same `idempotency_key`; duplicate delivery receipts
  update the existing row and do not create new approval decisions.
- Approval links and countdowns use server time. UI may display `ttl_seconds`,
  but execution checks `expires_at_server`.

`waiting_for_edge` timeout defaults to 300 seconds. On timeout, the run writes
`edge_timeout` and moves to `failed` or `waiting_for_user` according to a
per-run policy stored in `checkpoint_json.extra.waiting_for_edge_policy`.

<!-- /GAP-FIX: G7 -->

<!-- GAP-FIX: G4 -->

`delegation_state` is backed by a queryable projection table. The state item is
still useful for prompt assembly, but `session_delegations` is the contract for
UI tree rendering, parent/child resume, and drill-down APIs.

```sql
CREATE TABLE IF NOT EXISTS session_delegations (
  delegation_id                VARCHAR(128) PRIMARY KEY,
  user_id                      VARCHAR(128) NOT NULL,
  session_id                   VARCHAR(128) NOT NULL,
  parent_run_id                VARCHAR(128) NOT NULL,
  child_run_id                 VARCHAR(128) NOT NULL,
  child_session_id             VARCHAR(128) NULL,
  root_run_id                  VARCHAR(128) NOT NULL,
  ancestor_path                TEXT NOT NULL,
  depth                        INT NOT NULL DEFAULT 1,
  status                       VARCHAR(32) NOT NULL,
  phase                        VARCHAR(64) NULL,
  directive                    TEXT NULL,
  last_summary_ref             VARCHAR(128) NULL,
  last_summary_token_estimate  INT NOT NULL DEFAULT 0,
  exposed_artifacts_json       LONGTEXT NULL,
  deps_json                    LONGTEXT NULL,
  blocker_json                 LONGTEXT NULL,
  spawned_at                   TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  last_child_turn_idx          BIGINT NOT NULL DEFAULT -1,
  request_id                   VARCHAR(128) NULL,
  trace_id                     VARCHAR(128) NULL,
  created_at                   TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at                   TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  UNIQUE KEY uq_delegation_child_run (child_run_id),
  INDEX idx_delegations_session_status (session_id, status, updated_at),
  INDEX idx_delegations_parent (parent_run_id, status),
  INDEX idx_delegations_root_depth (root_run_id, depth, updated_at),
  INDEX idx_delegations_child_session (child_session_id)
);
```

Delegation contract:

- `agent_runs.parent_run_id`, `root_run_id`, and `ancestor_path` always define
  the execution tree. This exists even when the child is not shown as a
  separate session.
- `agent_sessions` is created for a child only when that child needs independent
  identity: user-visible tab, long-lived resume, separate permissions, or
  cross-session reuse. Most subagents are run children inside the parent
  session.
- Every child run with `trigger_type='delegation'` must have one
  `session_delegations` row.
- `last_summary_ref` points to the child session's
  `session_state_items(category='summary')` when `child_session_id` exists; for
  run-only children it points to a summary item in the parent session keyed by
  `delegation_id`.
- `session_state_items(category='delegation_state')` stores the compact prompt
  projection: child title/directive, status, phase, last summary ref, blocker,
  and exposed artifact refs. It should not embed the child raw transcript.
- Parent context assembly renders delegation as bounded structured summaries.
  Exact child details are fetched through explicit drill-down APIs, not by
  expanding every descendant into the prompt.

The JSON shape used by the `delegation_state` projection mirrors the table:

```json
{
  "delegation_id": "del_...",
  "child_session_id": "sess_child_or_null",
  "child_run_id": "run_child",
  "depth": 2,
  "root_run_id": "run_root",
  "ancestor_path": "run_root/run_parent/run_child",
  "status": "running",
  "phase": "investigating",
  "last_summary_ref": "state_summary_child",
  "last_summary_token_estimate": 180,
  "exposed_artifacts": [],
  "blocker": null,
  "deps": [],
  "directive": "Audit the migration path",
  "spawned_at": "2026-05-06T00:00:00Z",
  "last_child_turn_idx": 12
}
```

Minimum APIs:

- `GET /sessions/{id}/delegations?root_run_id=...`
- `GET /chat/runs/{run_id}/children`
- `GET /chat/runs/{run_id}/delegation-summary`

#### Projection Sync Contract

`session_state_items(category='delegation_state')` and `session_delegations` are
two views of the same fact. Writers must keep them in sync:

- Any create/update of a `session_delegations` row must happen in the same
  transaction as the matching `session_state_items` upsert.
- `item_key` for the state item uses `delegation:<delegation_id>` so joins are
  trivial.
- Closing a delegation transitions both rows: `session_delegations.status` moves
  to a terminal state and `session_state_items.status='archived'`.
- The `session_delegations` row is the source of truth for tree structure
  (`depth`, `root_run_id`, `ancestor_path`); the state item is the prompt
  projection tuned for `delegation_state` zone rendering.

<!-- /GAP-FIX: G4 -->

### 3. Context Manifests

The manifest is the debuggable contract between durable state and the LLM
request.

```sql
CREATE TABLE IF NOT EXISTS context_manifests (
  manifest_id             VARCHAR(128) PRIMARY KEY,
  user_id                 VARCHAR(128) NOT NULL,
  session_id              VARCHAR(128) NOT NULL,
  run_id                  VARCHAR(128) NULL,
  turn_id                 VARCHAR(128) NOT NULL,
  model_provider          VARCHAR(64) NOT NULL,
  model_name              VARCHAR(128) NOT NULL,
  context_window_tokens   INT NOT NULL,
  max_output_tokens       INT NOT NULL,
  total_estimated_tokens  INT NOT NULL,
  stable_prefix_hash      VARCHAR(128) NULL,
  prompt_cache_key        VARCHAR(255) NULL,
  compaction_version      VARCHAR(64) NULL,
  policy_version          VARCHAR(64) NOT NULL,
  tokenizer_id            VARCHAR(128) NULL,
  budget_template_id      VARCHAR(64) NULL,
  turn_intent             VARCHAR(64) NULL,
  reason                  VARCHAR(64) NOT NULL,
  dropped_count           INT NOT NULL DEFAULT 0,
  manifest_json           LONGTEXT NOT NULL,
  request_id              VARCHAR(128) NULL,
  trace_id                VARCHAR(128) NULL,
  created_at              TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  INDEX idx_ctx_manifest_session_turn (session_id, turn_id),
  INDEX idx_ctx_manifest_run (run_id),
  INDEX idx_ctx_manifest_user_created (user_id, created_at)
);

<!-- GAP-FIX: G1 -->

CREATE TABLE IF NOT EXISTS context_manifest_reason_types (
  reason          VARCHAR(64) PRIMARY KEY,
  reason_class    VARCHAR(64) NOT NULL,
  description     TEXT NOT NULL,
  default_zone    VARCHAR(64) NULL,
  is_active       BOOLEAN NOT NULL DEFAULT TRUE,
  created_at      TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  INDEX idx_ctx_reason_class (reason_class, reason)
);

CREATE TABLE IF NOT EXISTS context_manifest_items (
  id                 BIGINT AUTO_INCREMENT PRIMARY KEY,
  manifest_id        VARCHAR(128) NOT NULL,
  session_id         VARCHAR(128) NOT NULL,
  item_order         INT NOT NULL,
  zone               VARCHAR(64) NOT NULL,
  source_table       VARCHAR(64) NOT NULL,
  source_id          VARCHAR(128) NOT NULL,
  source_hash        VARCHAR(128) NULL,
  included           BOOLEAN NOT NULL,
  token_estimate     INT NOT NULL DEFAULT 0,
  budget_tokens      INT NOT NULL DEFAULT 0,
  reason             VARCHAR(128) NOT NULL,
  render_mode        VARCHAR(64) NOT NULL,
  created_at         TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  UNIQUE KEY uq_manifest_item_order (manifest_id, item_order),
  INDEX idx_manifest_items_source (source_table, source_id),
  INDEX idx_manifest_items_session_zone (session_id, zone, included)
);
```

`context_manifests.reason` is a logical foreign key to
`context_manifest_reason_types.reason`. MatrixOne deployments that cannot
enforce foreign keys should still seed and validate this enum in Rust before
writing a manifest. Free-form reason strings are not allowed in production.

Initial reason enum:

| reason | class | default zone |
| --- | --- | --- |
| `initial_turn` | lifecycle | `session_anchor` |
| `normal_turn` | lifecycle | `recent_tail` |
| `post_compaction` | compaction | `summary` |
| `history_recall_structured` | retrieval | `retrieved_facts` |
| `history_recall_fts` | retrieval | `retrieved_facts` |
| `history_recall_vector` | retrieval | `retrieved_facts` |
| `large_tool_output_gated` | artifact | `tool_previews` |
| `plan_subtree_query` | plan | `plan_todo` |
| `tree_structured_report` | plan | `plan_todo` |
| `workspace_switch` | workspace | `workspace` |
| `approval_resume` | approval | `safety_approvals` |
| `cross_session_recall` | retrieval | `retrieved_facts` |
| `delegation_poll` | delegation | `delegation_state` |
| `partial_blocker_review` | delegation | `delegation_state` |
| `delegation_aggregate` | delegation | `delegation_state` |
| `cross_skill_alignment` | skills | `skills` |
| `skill_quality_review` | skills | `skills` |
| `final_delivery_summary` | lifecycle | `summary` |

Reason selection must reflect the highest-cost or most-specific behavior in the
turn. For example, a normal user message that also loads old chunks uses
`history_recall_structured` / `history_recall_fts` / `history_recall_vector`;
a multi-agent parent turn that polls children uses `delegation_poll`, not
`normal_turn`.

<!-- GAP-FIX: G26 -->

Sprint D extends the manifest reason enum for turn intents discovered during
end-to-end walkthroughs:

| reason | class | default zone | Use when |
| --- | --- | --- | --- |
| `ambiguity_clarification` | next_action | `plan_todo` | User input produced multiple plausible structured continuations and the server asks for clarification. |
| `execute_after_clarification` | next_action | `plan_todo` | User selected a pending suggestion and the next turn executes that choice. |
| `user_memory_promote` | memory | `session_anchor` | A session fact is promoted to user/project/workspace scope. |
| `user_memory_archive` | memory | `session_anchor` | A durable user memory is archived or made inactive. |
| `user_memory_revise` | memory | `session_anchor` | A durable user memory is edited or superseded. |
| `user_memory_loaded_on_init` | memory | `session_anchor` | New or resumed session loaded user-scope memory into the anchor zone. |
| `progressive_loading` | budget | NULL | A small-window turn intentionally loads only the first slice of several relevant sources. |
| `intent_driven_preview_expand` | budget | `tool_previews` | A turn intent such as benchmark comparison needs extra structured previews. |
| `other` | fallback | NULL | Last-resort fallback for unknown reason values after alerting. |

`cross_skill_alignment` remains the canonical skills reason for temporarily
loading a reference skill beside an active skill.

Unknown reason fallback:

- Production writers must validate against `context_manifest_reason_types`.
- If an internal component proposes an unknown reason at runtime, the server may
  write `reason='other'` only after emitting `agent_events.event_type=
  'manifest.reason_unknown'` with `proposed_reason`, `turn_id`, `run_id`, and
  `component`.
- `manifest_json` must preserve `reason_original` so audit can identify the
  missing enum, but SQL grouping remains stable on `other`.

`turn_intent` is a separate optional field. It describes the user's work mode
inside a reason class and may drive budget overrides without multiplying reason
values. Seeded values:

| turn_intent | Budget behavior |
| --- | --- |
| `normal` | No override. |
| `benchmark_comparison` | May raise `tool_previews` up to 2500 tokens by borrowing from `recent_tail` while respecting the `recent_tail` floor. |
| `citation_verification` | Prefer citation/benchmark previews over broad recent tail. |
| `ambiguity_resolution` | Reserve budget for candidate summaries and source refs. |
| `progressive_loading` | Render the first source slice and leave explicit continuation refs in `context_manifest_items`. |
| `skill_alignment` | Render active skill plus bounded reference skills in the skills zone. |

When an intent override changes a zone cap, `manifest_json` records
`budget_override={zone, base_cap, override_cap, actual_tokens, borrowed_from}`.

<!-- /GAP-FIX: G26 -->

<!-- /GAP-FIX: G1 -->

Example manifest zones:

- `system_static`: product/system/developer prompts and safety contract.
- `tool_schemas`: selected tool schemas after pruning.
- `skills`: selected skills and personal skill snippets.
- `session_anchor`: L0/L1 structured state.
- `plan_todo`: active plan and current todos.
- `recent_tail`: recent uncompressed conversation turns.
- `summary`: compacted older conversation.
- `retrieved_facts`: relevant decisions, files, errors, memories.
- `delegation_state`: bounded summaries of active child agents.
- `tool_previews`: shortened tool outputs with artifact references.
- `safety_approvals`: pending approvals, requester confirmations, and blocked
  tools.
- `workspace`: branch, sandbox, edge/cloud workspace metadata.

The LLM sees rendered text/messages derived from this manifest. The database
keeps the manifest and item provenance so a failed or expensive turn can be
explained later.

### 4. Tool Result and Artifact References

`session_artifacts` already exists. The missing rule is that large tool results
must not live only in model messages. Add or extend artifact rows with a tool
result type and a prompt preview.

Recommended projection columns if extending `session_artifacts` is not enough:

```sql
CREATE TABLE IF NOT EXISTS session_tool_outputs (
  output_id        VARCHAR(128) PRIMARY KEY,
  user_id          VARCHAR(128) NOT NULL,
  session_id       VARCHAR(128) NOT NULL,
  run_id           VARCHAR(128) NULL,
  event_id         VARCHAR(128) NULL,
  batch_id         VARCHAR(128) NULL,
  batch_seq        INT NULL,
  batch_row_idx    INT NULL,
  parent_output_id VARCHAR(128) NULL,
  tool_name        VARCHAR(128) NOT NULL,
  preview_template_version VARCHAR(64) NULL,
  normalize_version VARCHAR(16) NULL,
  status           VARCHAR(32) NOT NULL,
  title            VARCHAR(255) NULL,
  preview_text     TEXT NULL,
  preview_token_estimate INT NOT NULL DEFAULT 0,
  preview_status   VARCHAR(32) NOT NULL DEFAULT 'ok',
  max_preview_bytes INT NOT NULL DEFAULT 400,
  artifact_ref     VARCHAR(255) NULL,
  content_hash     VARCHAR(128) NULL,
  relevance_score  DECIMAL(8,4) NULL,
  http_status      INT NULL,
  content_type     VARCHAR(128) NULL,
  artifact_kind    VARCHAR(64) NULL,
  row_count        BIGINT NULL,
  error_count      BIGINT NULL,
  duration_ms      BIGINT NULL,
  byte_size        BIGINT NOT NULL DEFAULT 0,
  token_estimate   INT NOT NULL DEFAULT 0,
  request_id       VARCHAR(128) NULL,
  trace_id         VARCHAR(128) NULL,
  created_at       TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  INDEX idx_tool_outputs_session_created (session_id, created_at, output_id),
  INDEX idx_tool_outputs_tool_created (tool_name, created_at),
  INDEX idx_tool_outputs_session_tool_score (session_id, tool_name, relevance_score, created_at),
  INDEX idx_tool_outputs_status_created (session_id, status, created_at),
  INDEX idx_tool_outputs_batch (batch_id, batch_row_idx),
  INDEX idx_tool_outputs_parent (parent_output_id, created_at)
);

CREATE TABLE IF NOT EXISTS preview_template_registry (
  tool_name                 VARCHAR(128) NOT NULL,
  version                   VARCHAR(64) NOT NULL,
  status                    VARCHAR(32) NOT NULL DEFAULT 'active',
  max_preview_bytes         INT NOT NULL DEFAULT 400,
  default_chunk_type        VARCHAR(64) NOT NULL DEFAULT 'tool_output_preview',
  first_class_columns_json  LONGTEXT NOT NULL,
  fts_field_weights_json    LONGTEXT NOT NULL,
  normalize_version         VARCHAR(16) NOT NULL DEFAULT 'v1',
  schema_json               LONGTEXT NOT NULL,
  created_at                TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at                TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY (tool_name, version),
  INDEX idx_preview_templates_status (tool_name, status, updated_at)
);
```

Context assembly includes `preview_text` and `artifact_ref`, not the whole raw
output.

<!-- GAP-FIX: G23 -->

#### Tool Output Batch Insert Contract

Fan-out tools must not write one network round trip per output row. Any tool
executor that emits more than 50 `session_tool_outputs` rows in one turn uses a
batch contract.

```sql
CREATE TABLE IF NOT EXISTS session_tool_output_batches (
  batch_id            VARCHAR(128) PRIMARY KEY,
  user_id             VARCHAR(128) NOT NULL,
  session_id          VARCHAR(128) NOT NULL,
  run_id              VARCHAR(128) NULL,
  event_id            VARCHAR(128) NULL,
  tool_name           VARCHAR(128) NOT NULL,
  batch_seq           INT NOT NULL,
  status              VARCHAR(32) NOT NULL DEFAULT 'pending',
  expected_row_count  INT NOT NULL,
  inserted_row_count  INT NOT NULL DEFAULT 0,
  preview_bytes       BIGINT NOT NULL DEFAULT 0,
  raw_bytes           BIGINT NOT NULL DEFAULT 0,
  started_at          TIMESTAMP NULL,
  completed_at        TIMESTAMP NULL,
  duration_ms         BIGINT NULL,
  error_code          VARCHAR(128) NULL,
  error_message       TEXT NULL,
  request_id          VARCHAR(128) NULL,
  trace_id            VARCHAR(128) NULL,
  created_at          TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at          TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  UNIQUE KEY uq_tool_output_batch_seq (run_id, tool_name, batch_seq),
  INDEX idx_tool_output_batches_session (session_id, tool_name, status, created_at),
  INDEX idx_tool_output_batches_run_status (run_id, status, batch_seq)
);
```

Batch boundary:

- Recommended batch size is 100-500 rows.
- Hard maximum is 500 rows or 16 MiB of preview/metadata payload per DB
  transaction, whichever is reached first.
- A tool may write one aggregate output row plus many detail rows. The aggregate
  row is not eligible for context rendering until all required detail batches are
  `completed`, unless it declares `aggregation_complete=false` in its preview.

Write protocol:

```text
begin transaction
  insert session_tool_output_batches(status='writing', expected_row_count=N)
  bulk insert N session_tool_outputs rows with the same batch_id
  update session_tool_output_batches
    set status='completed', inserted_row_count=N, completed_at=now()
commit
```

Failure protocol:

- If any row in a batch fails validation, the transaction rolls back and no
  partial detail rows are visible.
- The executor writes one failed batch row with `status='failed'` and a structured
  `error_code` in a separate transaction.
- Context assembly must ignore outputs whose batch is not `completed`.

Performance contract:

- The contract test target is 1000 `session_tool_outputs` rows in `<500ms` on
  the supported MatrixOne deployment, excluding raw artifact upload time.
- Implementations must use the SQLx/MySQL bulk insert path or equivalent
  multi-row insert. Per-row insert loops are a contract violation.
- Batch writes must expose `request_id` and `trace_id` on both batch and output
  rows so slow write audits can locate the exact executor call.

<!-- /GAP-FIX: G23 -->

<!-- GAP-FIX: G8 -->

#### Preview Template Registry

Every high-volume or large-output tool should declare a `preview_template.yaml`.
The runtime validates output previews against the active template before writing
`session_tool_outputs.preview_text`. Tools without a template use a 400
character fallback preview and `preview_status='fallback'`.

Template fields:

```yaml
tool_name: fetch_url
version: v1
max_preview_bytes: 1000
default_chunk_type: artifact_text
normalize_version: v1
first_class_columns:
  - name: http_status
    type: int
  - name: relevance_score
    type: decimal
  - name: content_type
    type: string
fts_field_weights:
  title: 4
  first_paragraph: 2
  keywords: 3
schema:
  required: [url, http_status, title, first_paragraph, keywords, content_type]
```

Baseline templates:

| tool | max preview | first-class columns | chunk type |
| --- | --- | --- | --- |
| `pg_dump` | 1200 bytes | `artifact_kind`, `row_count`, `content_hash` | `artifact_text` |
| `slow_query_analyzer` | 1600 bytes | `row_count`, `error_count`, `duration_ms` if available | `tool_output_preview` |
| `fetch_url` | 1000 bytes | `http_status`, `content_type`, `relevance_score` | `artifact_text` |
| `parse_pdf` | 1400 bytes | `content_type`, `relevance_score`, `row_count` | `artifact_text` |
| `llm_extract_findings` | 1200 bytes | `relevance_score` | `finding` |
| `benchmark_slice` | 1200 bytes | `relevance_score`, `row_count` | `benchmark` |

`finding`, `benchmark`, and `citation` payloads are structured first-class facts:

- `finding`: `{claim, evidence_refs[], confidence, source_artifact_ref}`
- `benchmark`: `{metric, value, unit, baseline, method, source_artifact_ref}`
- `citation`: `{source_artifact_ref, locator, quote_hash, summary}`

These categories can be emitted by preview extraction and are protected by
compaction invariants. Cross-agent promotion of these facts uses normal
`insert`/`update` for Sprint B; tree-level `bubble_up` and `apply_suggestion`
remain G14.

<!-- /GAP-FIX: G8 -->

<!-- GAP-FIX: G27 -->

#### Tool Baseline, Raw Ref, and Executor Registration

The baseline registry is a product contract, not sample documentation. Tool
executors must register their preview template and normalization version before
they can write `session_tool_outputs` in production.

```sql
CREATE TABLE IF NOT EXISTS tool_executor_registry (
  tool_name                 VARCHAR(128) PRIMARY KEY,
  executor_version          VARCHAR(64) NOT NULL,
  preview_template_version  VARCHAR(64) NOT NULL,
  normalize_version         VARCHAR(16) NOT NULL DEFAULT 'raw_v1',
  default_raw_ref_scheme    VARCHAR(64) NOT NULL,
  status                    VARCHAR(32) NOT NULL DEFAULT 'active',
  created_at                TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at                TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  INDEX idx_tool_executor_status (status, updated_at)
);

CREATE TABLE IF NOT EXISTS raw_ref_scheme_registry (
  scheme              VARCHAR(64) PRIMARY KEY,
  resolver_name       VARCHAR(128) NOT NULL,
  backing_store       VARCHAR(64) NOT NULL,
  access_check        VARCHAR(64) NOT NULL,
  canonical_example   VARCHAR(255) NOT NULL,
  is_active           BOOLEAN NOT NULL DEFAULT TRUE,
  created_at          TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);
```

Canonical `raw_ref` format:

```text
<scheme>://<namespace>/<id>@<content_hash>
```

Registered baseline schemes:

| scheme | Example | Resolver |
| --- | --- | --- |
| `artifact` | `artifact://session/s_123/artifacts/art_456@sha256:abc` | `session_artifact_resolver` |
| `conversation_log` | `conversation_log://session/s_123/item/9182@sha256:def` | `conversation_log_resolver` |
| `object_store` | `object_store://bucket/key?version=v7@sha256:abc` | `object_store_resolver` |
| `s3` | `s3://bucket/key?versionId=v7@sha256:abc` | `s3_resolver` |
| `cold_storage` | `cold_storage://archive/session/s_123/art_456@sha256:abc` | `cold_storage_resolver` |
| `blob` | `blob://sha256/abc` | `blob_resolver` |

Resolvers must perform the G9 access check before loading raw bytes. String
parsing alone never grants access.

Expanded baseline templates:

| tool | max preview | first-class columns | chunk type | normalize |
| --- | ---: | --- | --- | --- |
| `pg_dump` | 1200 bytes | `artifact_kind`, `row_count`, `content_hash` | `artifact_text` | `pg_dump_v1` |
| `pg_schema_structurize` | 1600 bytes | `row_count`, `error_count`, `artifact_kind` | `artifact_text` | `pg_schema_struct_v1` |
| `sql_compat_scan` | 1600 bytes aggregate / 400 bytes detail | `row_count`, `error_count`, `artifact_kind`, `relevance_score` | `tool_output_preview` | `sql_compat_scan_v1` |
| `slow_query_analyzer` | 1600 bytes | `row_count`, `error_count`, `duration_ms` | `tool_output_preview` | `slow_query_v1` |
| `slow_query_explain` | 1200 bytes | `duration_ms`, `relevance_score` | `tool_output_preview` | `slow_query_explain_v1` |
| `fetch_url` | 1000 bytes | `http_status`, `content_type`, `relevance_score` | `artifact_text` | `fetch_url_v1` |
| `parse_pdf` | 1400 bytes | `content_type`, `relevance_score`, `row_count` | `artifact_text` | `parse_pdf_v1` |
| `pdf_text_section_read` | 1200 bytes | `relevance_score`, `row_count` | `artifact_text` | `pdf_section_v1` |
| `llm_extract_findings` | 1200 bytes | `relevance_score` | `finding` | `finding_v1` |
| `benchmark_slice` | 1200 bytes | `relevance_score`, `row_count` | `benchmark` | `benchmark_slice_v1` |
| `cargo` | 2000 bytes | `error_count`, `warning_count`, `artifact_kind` | `error` | `cargo_v1` |
| `rustc` | 1000 bytes | `error_count`, `artifact_kind` | `error` | `rustc_v1` |
| `clippy` | 1200 bytes | `error_count`, `warning_count`, `artifact_kind` | `error` | `clippy_v1` |
| `edge_fs_read` | 1000 bytes | `artifact_kind`, `byte_size` | `artifact_text` | `fs_read_v1` |
| `edge_fs_write` | 1200 bytes | `artifact_kind`, `error_count` | `artifact_text` | `fs_write_v1` |
| `paste_code` | 1200 bytes | `artifact_kind`, `byte_size` | `artifact_text` | `paste_code_v1` |
| `eslint` | 1200 bytes | `error_count`, `warning_count`, `artifact_kind` | `error` | `eslint_v1` |
| `skill_diff` | 1200 bytes | `artifact_kind`, `relevance_score` | `artifact_text` | `skill_diff_v1` |

Normalization rules:

- New writes must set `normalize_version`; `NULL` is deprecated.
- Existing `NULL` values are interpreted as `raw_v1`.
- `raw_v1` is the identity transform: `sha256(raw_bytes)`. It is the required
  choice for tools such as raw slowlog capture where normalization would destroy
  forensic value.
- If an executor bumps `preview_template_version` without bumping
  `normalize_version`, the template changelog must state that the change is
  display-only and does not affect hash semantics.

Tool-output provenance:

- `session_tool_outputs.parent_output_id` links a derived output to the output
  that caused it, such as an `explain` row derived from a slow-query row.
- Multi-source artifact derivation uses a child table instead of JSON filters:

```sql
CREATE TABLE IF NOT EXISTS session_artifact_provenance (
  artifact_id          VARCHAR(128) NOT NULL,
  source_artifact_id   VARCHAR(128) NOT NULL,
  relation_type        VARCHAR(64) NOT NULL,
  created_at           TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY (artifact_id, source_artifact_id, relation_type),
  INDEX idx_artifact_provenance_source (source_artifact_id, created_at)
);
```

`session_artifacts.derived_from_artifact_id` remains the fast single-source
pointer; `session_artifact_provenance` is authoritative for multi-source
reports.

<!-- /GAP-FIX: G27 -->

<!-- GAP-FIX: G9 -->

#### Artifact Retention and Access Scope

`session_artifacts` should be extended with retention and reverse-reference
counters. If the existing table already has equivalent columns, keep them and
map the names explicitly in the migration.

```sql
-- Additive extension to the existing session_artifacts table.
ALTER TABLE session_artifacts ADD COLUMN project_id VARCHAR(128) NULL;
ALTER TABLE session_artifacts ADD COLUMN access_scope VARCHAR(32) NOT NULL DEFAULT 'delegation';
ALTER TABLE session_artifacts ADD COLUMN retention_policy VARCHAR(32) NOT NULL DEFAULT 'default';
ALTER TABLE session_artifacts ADD COLUMN retention_until TIMESTAMP NULL;
ALTER TABLE session_artifacts ADD COLUMN status VARCHAR(32) NOT NULL DEFAULT 'active';
ALTER TABLE session_artifacts ADD COLUMN normalize_version VARCHAR(16) NULL;
ALTER TABLE session_artifacts ADD COLUMN cold_storage_ref VARCHAR(255) NULL;
ALTER TABLE session_artifacts ADD COLUMN derived_from_artifact_id VARCHAR(128) NULL;
ALTER TABLE session_artifacts ADD COLUMN referenced_by_manifest_count INT NOT NULL DEFAULT 0;
ALTER TABLE session_artifacts ADD COLUMN referenced_by_state_items_count INT NOT NULL DEFAULT 0;
ALTER TABLE session_artifacts ADD COLUMN referenced_by_citation_count INT NOT NULL DEFAULT 0;
ALTER TABLE session_artifacts ADD INDEX idx_artifacts_retention (status, retention_until, retention_policy);
ALTER TABLE session_artifacts ADD INDEX idx_artifacts_project (project_id, status, retention_until);
ALTER TABLE session_artifacts ADD INDEX idx_artifacts_derived (derived_from_artifact_id);

-- Additive extension to agent_sessions.
ALTER TABLE agent_sessions ADD COLUMN project_id VARCHAR(128) NULL;
ALTER TABLE agent_sessions ADD COLUMN project_retention_policy VARCHAR(32) NOT NULL DEFAULT 'session';
ALTER TABLE agent_sessions ADD INDEX idx_sessions_project (user_id, project_id, updated_at);
```

Retention policies:

- `default`: normal session-level retention.
- `project_long_term`: retained while the project is active.
- `permanent`: skipped by automatic GC until user/admin deletion.

Artifact statuses:

- `active`: raw bytes available in hot/warm storage.
- `expiring`: within the T-7 day GC preflight window.
- `archived_cold`: raw bytes moved to cold storage; `cold_storage_ref` is set.
- `expired`: raw bytes unavailable; summaries and previews remain.

Access scopes:

- `private`: only the owning session can load raw bytes.
- `delegation`: DEPRECATED in v0.3 as an ambiguous alias. Existing rows map to
  `same_root_tree` unless a migration explicitly narrows them to
  `delegation_direct`.
- `delegation_direct`: owning run, direct parent runs, and descendants in the
  same delegation branch can load raw bytes.
- `same_root_tree`: any run/session under the same `root_run_id` can load raw
  bytes when the artifact is exposed by the owner or parent orchestrator.
- `user`: all sessions owned by the same user can load raw bytes.

<!-- GAP-FIX: G20 -->

#### Delegation Tree Artifact ACL

Parallel child agents commonly need sibling-produced artifacts. The old v0.2
definition of `access_scope='delegation'` as "parent plus descendants" is too
narrow for multi-agent workflows and is deprecated.

Additive schema:

```sql
ALTER TABLE session_artifacts ADD COLUMN owner_run_id VARCHAR(128) NULL;
ALTER TABLE session_artifacts ADD COLUMN owner_delegation_id VARCHAR(128) NULL;
ALTER TABLE session_artifacts ADD COLUMN root_run_id VARCHAR(128) NULL;
ALTER TABLE session_artifacts ADD INDEX idx_artifacts_root_scope (root_run_id, access_scope, status, updated_at);
ALTER TABLE session_artifacts ADD INDEX idx_artifacts_owner_run (owner_run_id, status, updated_at);

ALTER TABLE session_delegations ADD COLUMN sibling_exposed_artifacts_json LONGTEXT NULL;

CREATE TABLE IF NOT EXISTS session_artifact_grants (
  grant_id              VARCHAR(128) PRIMARY KEY,
  artifact_id           VARCHAR(128) NOT NULL,
  user_id               VARCHAR(128) NOT NULL,
  session_id            VARCHAR(128) NOT NULL,
  root_run_id           VARCHAR(128) NOT NULL,
  source_run_id         VARCHAR(128) NOT NULL,
  target_run_id         VARCHAR(128) NULL,
  target_delegation_id  VARCHAR(128) NULL,
  grant_scope           VARCHAR(32) NOT NULL,
  granted_by            VARCHAR(128) NOT NULL,
  reason                VARCHAR(128) NULL,
  expires_at            TIMESTAMP NULL,
  created_at            TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at            TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  UNIQUE KEY uq_artifact_grant_target (artifact_id, grant_scope, target_run_id, target_delegation_id),
  INDEX idx_artifact_grants_root (root_run_id, grant_scope, created_at),
  INDEX idx_artifact_grants_target (target_run_id, target_delegation_id, created_at)
);
```

Access matrix:

| artifact.access_scope | Owner run | Direct parent | Descendant | Sibling under same root | Root orchestrator | Different root |
| --- | --- | --- | --- | --- | --- | --- |
| `private` | allow | deny | deny | deny | deny unless owner | deny |
| `delegation_direct` | allow | allow | allow | deny unless grant exists | allow if on branch | deny |
| `same_root_tree` | allow | allow | allow | allow when `root_run_id` matches | allow | deny |
| `user` | allow | allow if same user | allow if same user | allow if same user | allow if same user | allow if same user |

Grant rules:

- Sibling reads are allowed only when `artifact.root_run_id` equals the
  requesting run's `root_run_id` and either:
  - `access_scope='same_root_tree'`, or
  - a `session_artifact_grants` row targets the requesting run/delegation, or
  - the parent orchestrator lists the artifact in
    `session_delegations.sibling_exposed_artifacts_json`.
- `ancestor_path` prefix matching is a helper optimization, not the sole
  authority. The authorization check must use indexed `root_run_id` and explicit
  grant rows to avoid scanning or string-only authorization.
- Grant rows are append-only audit records. Revocation creates a new state/event
  record and sets `expires_at`; it does not delete the original grant.
- Context assembly may render a sibling artifact ref, but raw byte download
  still calls the same ACL check as direct API access.

Examples:

- DBA child exposes a migration SQL artifact with `same_root_tree`; BE sibling
  can read it because both runs share the PM root run.
- A reviewer under executor-2 exposes a critical finding; the reporter sibling
  can read the finding artifact after root orchestrator grant/bubble-up.
- A different user session under the same project still cannot read the artifact
  unless `access_scope='user'` and ownership policy allows it.

<!-- /GAP-FIX: G20 -->

GC contract:

1. At T-7 days before `retention_until`, mark candidate artifacts `expiring`.
2. Recompute manifest/state/citation counters from
   `context_manifest_items`, `session_state_items`, and `citation` items.
3. If any counter is non-zero, extend retention or migrate to cold storage.
4. GC derived artifacts only after their source chain is safe to expire.
5. If raw bytes expire, keep a tombstone row with `status='expired'`.

Context builder behavior for expired artifacts:

```text
artifact_ref=artifact_123
status=expired
  -> render: "historical artifact; raw no longer available; summary preserved"
  -> include summary/preview refs only
  -> never fail the entire context assembly
```

Large artifact downloads should return a presigned object-store URL. The API
server signs and audits the request; it should not proxy multi-GB object bytes.

Cross-gap note: citation counters depend on G8's `citation` payload schema.
Delegation-tree sharing uses G4/G18 access paths. G14 may later add `bubble_up`
events that increase `referenced_by_state_items_count`, but G9 does not require
that mutation to define retention.

<!-- /GAP-FIX: G9 -->

### 5. Transcript Projection

Web history browsing and runtime context reconstruction are different flows.
For Web UI, add a display-optimized transcript projection. This avoids scanning
or materializing a large `conversation_log` every time the user scrolls.

```sql
CREATE TABLE IF NOT EXISTS session_transcript_items (
  session_id        VARCHAR(128) NOT NULL,
  item_seq          BIGINT NOT NULL,
  user_id           VARCHAR(128) NOT NULL,
  turn              BIGINT NULL,
  run_id            VARCHAR(128) NULL,
  message_id        VARCHAR(128) NULL,
  part_id           VARCHAR(128) NULL,
  event_id          VARCHAR(128) NULL,
  item_type         VARCHAR(64) NOT NULL,
  role              VARCHAR(32) NULL,
  created_at        TIMESTAMP NOT NULL,
  preview_text      TEXT NULL,
  payload_ref       VARCHAR(255) NULL,
  payload_hash      VARCHAR(128) NULL,
  token_estimate    INT NOT NULL DEFAULT 0,
  is_compacted      BOOLEAN NOT NULL DEFAULT FALSE,
  is_deleted        BOOLEAN NOT NULL DEFAULT FALSE,
  request_id        VARCHAR(128) NULL,
  trace_id          VARCHAR(128) NULL,
  PRIMARY KEY (session_id, item_seq),
  INDEX idx_transcript_user_created (user_id, created_at, session_id),
  INDEX idx_transcript_session_created (session_id, created_at, item_seq),
  INDEX idx_transcript_message (message_id),
  INDEX idx_transcript_event (event_id)
);
```

Important details:

- `item_seq` is a session-local display sequence. It should be monotonic and
  stable.
- This table stores UI preview and references, not necessarily the full payload.
- Large payloads live in artifacts/tool output storage and are loaded on demand.
- Deletes/compaction should usually mark rows, not rewrite history. The UI can
  hide or show compacted/deleted rows based on mode.
- Cursor pagination uses `(session_id, item_seq)`, not offset pagination.

Web scroll-up flow:

```text
User scrolls up
  ↓
GET /sessions/{id}/transcript?before_seq=12345&limit=50
  ↓
SELECT ... FROM session_transcript_items
WHERE session_id = ? AND item_seq < ?
ORDER BY item_seq DESC
LIMIT 50
  ↓
Server returns display items + next before_seq cursor
```

This is an indexed range read. It does not read the full session history and
does not filter inside JSON.

Complete UI history reconstruction:

```text
SELECT ...
FROM session_transcript_items
WHERE session_id = ?
ORDER BY item_seq ASC
```

This can reproduce display order completely. It is still paged for Web clients.
Export/audit jobs may stream it in batches.

### 6. History Chunks and Retrieval Index

For a 10GB session, the LLM cannot receive all history, and a summary alone is
not enough to preserve details. The design needs a retrieval layer that can find
old raw slices by structure, text, and semantics.

```sql
CREATE TABLE IF NOT EXISTS session_history_chunks (
  chunk_id          VARCHAR(128) PRIMARY KEY,
  user_id           VARCHAR(128) NOT NULL,
  session_id        VARCHAR(128) NOT NULL,
  seq_start         BIGINT NOT NULL,
  seq_end           BIGINT NOT NULL,
  item_seq_start    BIGINT NULL,
  item_seq_end      BIGINT NULL,
  turn_start        BIGINT NULL,
  turn_end          BIGINT NULL,
  chunk_type        VARCHAR(64) NOT NULL,
  source_table      VARCHAR(64) NOT NULL,
  source_id         VARCHAR(128) NOT NULL,
  title             VARCHAR(255) NULL,
  preview_text      TEXT NULL,
  raw_ref           VARCHAR(255) NOT NULL,
  content_hash      VARCHAR(128) NOT NULL,
  token_estimate    INT NOT NULL DEFAULT 0,
  importance        INT NOT NULL DEFAULT 0,
  created_at        TIMESTAMP NOT NULL,
  indexed_at        TIMESTAMP NULL,
  request_id        VARCHAR(128) NULL,
  trace_id          VARCHAR(128) NULL,
  INDEX idx_history_session_seq (session_id, seq_start, seq_end),
  INDEX idx_history_session_created (session_id, created_at, chunk_id),
  INDEX idx_history_type_created (session_id, chunk_type, created_at),
  INDEX idx_history_user_type_created (user_id, chunk_type, created_at, session_id),
  INDEX idx_history_source (source_table, source_id)
);
```

`chunk_type` examples:

- `user_message`
- `assistant_message`
- `tool_call`
- `tool_output_preview`
- `artifact_text`
- `file_snapshot`
- `decision`
- `error`
- `plan_change`
- `todo_change`
- `summary`
- `finding`
- `benchmark`
- `citation`

Indexing policy:

- Structured indexes live in normal columns: `session_id`, `seq_start`,
  `created_at`, `chunk_type`, `source_table`, `source_id`.
- Full-text index should target `preview_text` or a separate text index table.
- Vector embeddings should reference `chunk_id` and be batch inserted. Do not
  update/delete vector rows frequently; append versions and soft-delete stale
  rows if needed.
- Raw content is addressed by `raw_ref` and `content_hash`; it can point back to
  `conversation_log`, `session_artifacts`, object storage, or a compressed blob.

LLM old-detail retrieval flow:

```text
New user message
  ↓
ContextAssembler detects need for history retrieval
  ↓
Build retrieval request:
  - session_id
  - current task/plan/todo
  - query terms
  - optional time/turn/tool/file filters
  ↓
Retrieve candidates:
  - structured filters over session_history_chunks
  - full-text search over chunk text
  - vector search over chunk embeddings
  ↓
Rerank by relevance, recency, importance, source reliability, token cost
  ↓
Load raw slices for top results via raw_ref
  ↓
Render concise excerpts with provenance
  ↓
Persist selected chunks in context_manifest_items
  ↓
LLM receives only selected old details
```

Key rule:

> Summaries help locate and compress old history. Raw chunks remain the source
> of truth for details.

If the user asks "what exact error did we see last month?", retrieval should use
summary/indexes to find candidate chunks, then load the exact raw error slice
before rendering context.

<!-- GAP-FIX: G3 -->

#### Retrieval State Machine

Retrieval is staged and bounded. Each stage either returns enough candidates,
times out, or emits a typed degradation event before the assembler moves to the
next stage.

| Stage | Query path | Target SLA | Hard cap | Failure/degradation events |
| --- | --- | --- | --- | --- |
| Structured | Indexed columns on `session_history_chunks` | `<50ms` | `1000` scanned rows / `50` candidates | `retrieval.structured_empty`, `retrieval.structured_timeout`, `retrieval.bound_exceeded` |
| Full-text | FTS over indexed preview text | `<200ms` | `100` candidates | `retrieval.fts_empty`, `retrieval.fts_timeout` |
| Vector | Embedding table joined back to chunks by `chunk_id` + hash | `<500ms` | `1` vector query per turn / `20` candidates | `retrieval.vector_empty`, `retrieval.vector_timeout`, `retrieval.vector_stale` |
| Raw load | `raw_ref` for selected chunks only | `<250ms` hot; cold storage may exceed this | `top_k` selected refs | `retrieval.raw_missing`, `retrieval.raw_cold_fetch_required` |

End-to-end interactive retrieval target is `<1s` excluding explicitly cold
artifact fetches. If all stages fail or time out, the assistant must say the
old detail was not found instead of inventing continuity from a summary.

State machine:

```text
start
  -> structured
  -> enough? render
  -> empty/ambiguous/timeout? emit event and try fts
  -> enough? render
  -> empty/ambiguous/timeout? emit event and try vector if turn budget allows
  -> vector stale? emit event, skip stale rows, enqueue reindex, fall back to fts/structured results
  -> selected refs? raw load selected refs
  -> persist context_manifest_items + retrieval events
```

`agent_events` retrieval event payload contract:

```json
{
  "stage": "vector",
  "reason": "stale",
  "session_id": "sess_...",
  "source_session_id": "sess_origin_or_same",
  "candidate_count": 12,
  "elapsed_ms": 183,
  "timeout_ms": 500,
  "query_hash": "sha256:...",
  "chunk_id": "chunk_...",
  "content_hash": "sha256:chunk-current",
  "index_hash": "sha256:embedding-built-from",
  "fallback_stage": "fts"
}
```

Vector rows are append-only versions. When `session_history_chunks.content_hash`
does not match the vector row's indexed hash, retrieval must emit
`retrieval.vector_stale`, skip that vector row, and enqueue re-embedding. It
must not return stale semantic results.

<!-- /GAP-FIX: G3 -->

### 7. History Reconstruction Modes

The design supports three reconstruction modes:

| Mode | Source | Use case | Load pattern |
| --- | --- | --- | --- |
| Web display history | `session_transcript_items` + payload refs | Infinite scroll, export display transcript | Cursor pages by `item_seq` |
| Runtime resume | latest CSL snapshot + deltas + `session_state_items` | Continue agent work | Bounded materialization |
| LLM old-topic recall | `session_transcript_items` pages plus indexed `session_history_chunks` where available | Answer "we discussed X earlier" without loading the full transcript | `session_history_search` -> `session_history_around`, or `session_history_page` by cursor |
| Audit replay | `conversation_log`, `agent_events`, artifacts | Debug/replay/compliance | Background streaming by `seq`/time |
| Delegation tree drill-down | `agent_runs(parent_run_id)` + `session_delegations` + `delegation_state` items | Inspect a child/leaf run or superseded retry branch | Index lookup by `root_run_id`/`parent_run_id`, then fetch summary/artifact refs |

The Web UI must use display history mode. Runtime resume must not replay a 10GB
session. Audit replay is allowed to read everything, but it should run as a
background/export/debug task, not as part of opening the session.

LLM recall flow for "long ago we discussed X":

1. The current turn starts with CSL + bounded recent tail + active projections.
2. If those inputs do not contain the requested detail, the LLM calls
   `session_history_search({query: "X", limit, scan_limit})`.
3. The tool returns compact item_seq anchors and cursor hints, not raw pages of
   the whole transcript.
4. The LLM calls `session_history_around({item_seq, radius})` for the most
   likely anchor before continuing the task.
5. If topic search is ambiguous, the LLM can page with
   `session_history_page({before_seq, limit})` or ask the user to choose among
   returned anchors.

### 8. MatrixOne Load Model

Normal Web scrolling should be cheap:

```sql
SELECT item_seq, item_type, role, created_at, preview_text, payload_ref
FROM session_transcript_items
WHERE session_id = ?
  AND item_seq < ?
  AND is_deleted = FALSE
ORDER BY item_seq DESC
LIMIT 50;
```

Expected behavior:

- index range scan on `(session_id, item_seq)`;
- bounded rows returned;
- no JSON filtering;
- no raw 10GB payload reads;
- large payloads fetched only when a user expands an item.

Heavy operations and where they belong:

| Operation | Handling |
| --- | --- |
| Build transcript projection for old sessions | Background migration job |
| Build full-text/vector indexes for 10GB history | Async batch indexer |
| Export complete session | Streaming job with cursor checkpoints |
| Audit replay | Debug/background worker |
| Rebuild context manifests | Offline/backfill or explicit debug endpoint |

This keeps the interactive web path predictable while preserving full history.

### 9. Personal Skills

Existing tables are close: `skills_registry`, `skill_installations`,
`skill_settings`, `skill_resource_bindings`, and `skill_user_credentials`.
The web agent needs an explicit user-authored skill version model.

```sql
CREATE TABLE IF NOT EXISTS user_skill_sources (
  source_id      VARCHAR(128) PRIMARY KEY,
  user_id        VARCHAR(128) NOT NULL,
  skill_name     VARCHAR(128) NOT NULL,
  visibility     VARCHAR(32) NOT NULL DEFAULT 'private',
  status         VARCHAR(32) NOT NULL DEFAULT 'active',
  created_at     TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at     TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  UNIQUE KEY uq_user_skill_name (user_id, skill_name),
  INDEX idx_user_skill_status (user_id, status, updated_at)
);

CREATE TABLE IF NOT EXISTS user_skill_versions (
  version_id        VARCHAR(128) PRIMARY KEY,
  source_id         VARCHAR(128) NOT NULL,
  user_id           VARCHAR(128) NOT NULL,
  skill_name        VARCHAR(128) NOT NULL,
  version           VARCHAR(64) NOT NULL,
  manifest_json     LONGTEXT NOT NULL,
  content_markdown  LONGTEXT NOT NULL,
  normalize_version VARCHAR(16) NOT NULL DEFAULT 'skill_md_v1',
  content_hash      VARCHAR(128) NOT NULL,
  summary_text      TEXT NULL,
  token_estimate    INT NOT NULL DEFAULT 0,
  status            VARCHAR(32) NOT NULL DEFAULT 'draft',
  created_by        VARCHAR(128) NOT NULL,
  request_id        VARCHAR(128) NULL,
  trace_id          VARCHAR(128) NULL,
  created_at        TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  UNIQUE KEY uq_user_skill_version (source_id, version),
  INDEX idx_user_skill_versions_active (user_id, skill_name, status, created_at)
);

CREATE TABLE IF NOT EXISTS user_skill_evaluations (
  evaluation_id        VARCHAR(128) PRIMARY KEY,
  source_id            VARCHAR(128) NOT NULL,
  version_id           VARCHAR(128) NOT NULL,
  user_id              VARCHAR(128) NOT NULL,
  run_id               VARCHAR(128) NULL,
  target_ref           VARCHAR(255) NULL,
  hits                 INT NOT NULL DEFAULT 0,
  suspects             INT NOT NULL DEFAULT 0,
  false_positives      INT NOT NULL DEFAULT 0,
  missed_by_design     INT NOT NULL DEFAULT 0,
  hit_rate             DECIMAL(8,4) NULL,
  false_positive_rate  DECIMAL(8,4) NULL,
  payload_json         LONGTEXT NULL,
  created_at           TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  INDEX idx_skill_eval_version_created (version_id, created_at),
  INDEX idx_skill_eval_source_created (source_id, created_at),
  INDEX idx_skill_eval_quality (user_id, false_positive_rate, created_at)
);

-- Additive extension to existing skill_installations.
ALTER TABLE skill_installations ADD COLUMN scope VARCHAR(32) NOT NULL DEFAULT 'user';
ALTER TABLE skill_installations ADD COLUMN session_id VARCHAR(128) NULL;
ALTER TABLE skill_installations ADD COLUMN workspace_id VARCHAR(128) NULL;
ALTER TABLE skill_installations ADD COLUMN auto_activate_on_topic_match BOOLEAN NOT NULL DEFAULT FALSE;
ALTER TABLE skill_installations ADD INDEX idx_skill_install_scope (user_id, scope, session_id, workspace_id);
```

Publish flow:

1. User edits a skill in web UI.
2. Server validates `SKILL.md` manifest and content.
3. Server creates an immutable `user_skill_versions` row.
4. Active version is indexed in `skills_registry` with `created_by`,
   `source='user'`, `is_public`, and `content_hash`.
5. `skill_installations` marks it installed for the same user.
6. Skill selection uses the same selector, but filters by user ownership,
   installation, workspace bindings, and token budget.

Runtime visibility contract:

- Standalone CLI discovers project-local filesystem skills from cwd walk-up
  paths (`.astra/skills`, `.claude/skills`, and `skills/`). These project-local
  skills are CLI-only unless imported into `skills_registry`.
- API-server local skills are discovered independently by the server from the
  server process HOME only: `~/.astra/skills` and `~/.claude/skills`. They have
  no per-user ACL and are treated as deployment-level public skills for both Web
  and server-backed CLI sessions.
- Web/runtime skill catalog queries use the union:
  `api_server_home_skills ∪ skills_registry(created_by = current_user) ∪
  skills_registry(is_public = 1)`. The DB predicate is reused by HTTP
  list/detail/version endpoints and the runtime resolver.
- Runtime turns build the resolver by default from that visible catalog.
  `allow_skills` is only a request-scoped filter over the visible catalog. The
  LLM receives request-active skills plus the session-scoped
  `<available_skills>` catalog. `discover_skills` performs targeted lookup for
  catalog entries that do not fit in the prompt budget; full `SKILL.md` content
  is injected only after the model calls the `skill` tool.
- Web composer skill tokens are per-turn selections. The
  composer clears the submitted skill tokens as soon as the turn is submitted;
  failures restore them so the same request can be retried.
- The composer exposes the same per-turn skill selection through both the `+`
  menu and slash command palette. Typing `/` at a word boundary opens a
  prefix-matched command list; typing whitespace closes it without selecting
  anything. v1 registers `kind='skill'` commands from the shared skill catalog,
  while the command model is intentionally generic (`skill` / `mode` / `action`)
  so later `/plan` or other execution-mode commands can reuse the same palette.
  Skill tokens are inserted at the current caret position in the editable
  message stream, so users can describe each selected skill in context instead
  of having all selected skills grouped at the start.
- If a user-owned and public skill share a logical name, "latest by name"
  resolution prefers the user-owned row before comparing `created_at`. This
  prevents a newly published public skill from shadowing a user's private skill.

Capability catalog contract:

- Runtime capability resolution is centralized behind `CapabilityCatalog` rather
  than duplicated in Web UI, runtime handlers, and CLI prompt assembly.
- `surface='web'` and `surface='cli_remote'` are server-executed surfaces. They
  see only server-executable tools plus server MCP tools, and they use the same
  server-visible skill catalog:
  `api_server_home_skills ∪ skills_registry(created_by = current_user) ∪
  skills_registry(is_public = 1)`.
- `surface='cli_local'` is edge-executed. It sees local CLI tools plus local MCP
  tools and uses a registry ordered as:
  `cli_project_and_home_skills > bundled_cli_skills > authenticated_server_catalog`.
  This means project-local CLI skills can override same-named server catalog
  skills for local CLI execution, but they are still invisible to Web until
  explicitly published/registered.
- API-server HOME skills are discoverable over the authenticated `/skills`
  catalog as full records, not just list rows, so a local CLI connected to a
  remote API server can load the same server-local skill body that Web can use.
- No surface silently claims a capability it cannot execute. If a tool lives in
  a client-side MCP process, Web cannot see it unless that MCP process is also
  mounted server-side or the user runs a local/edge-assisted CLI turn.
- Expected visibility by deployment shape:

| Deployment shape | Web sees | CLI local sees | CLI remote/thin sees |
| --- | --- | --- | --- |
| Remote API + remote MCP | Server tools, remote MCP, API-server HOME skills, visible DB skills | Client tools plus client project/home skills; visible DB/API-server catalog if authenticated | Same as Web |
| Remote API + client MCP | Server tools, API-server HOME skills, visible DB skills | Client tools, client MCP, client project/home skills, plus authenticated server catalog | Same as Web; client MCP is not visible |
| CLI runs on API host | Server-local HOME skills and server-side tools line up with Web because the client and server execution site are the same host | Same host local tools/MCP plus project-local skills | Same as Web |
| All-in-one local dev | Web sees the API server's HOME catalog and server tools; CLI local sees the same HOME skills plus project-local skills and local MCP | Same as previous cell | Same as Web |

Future MatrixOne full-text/vector indexes can be added for skill retrieval. Use
append-only versions; do not update historical skill content in place.

<!-- GAP-FIX: G16 -->

#### Personal Skill Activation and Evaluation

`skill_installations` means "available to the user/workspace/session"; it does
not automatically place the skill in every prompt.

Activation is explicit session state:

```json
{
  "scope": "session",
  "category": "active_skill",
  "item_key": "go-code-review",
  "payload_json": {
    "source_id": "skill_src_...",
    "version_id": "skill_ver_v4",
    "content_hash": "sha256:...",
    "activation_source": "user_explicit"
  }
}
```

Rules:

- `version_id` is frozen at activation time. Later `skills_registry.active_version`
  changes do not mutate old sessions.
- New sessions do not auto-load installed user skills unless
  `auto_activate_on_topic_match=true`; otherwise the agent may suggest
  activation through `suggested_next_action`.
- Skills zone renders only active versions' `content_markdown`.
- The most recent `N=2` `user_skill_evaluations` for an active version may be
  rendered as warm context; older evaluations remain queryable but out of
  prompt.
- Versions can be `draft`, `published`, `superseded`, or `quarantined`.
  Quarantined versions cannot be auto-activated.

`user_skill_evaluations` is the aggregation table for trial quality. It stays
out of `session_state_items` so quality gates can query real columns such as
`false_positive_rate`.

<!-- /GAP-FIX: G16 -->

<!-- GAP-FIX: G17 -->

#### Content Hash Normalization Contract

Every tool output and skill source that writes `content_hash` must declare a
`normalize_version`. The hash input is always:

```text
sha256(normalize_<normalize_version>(raw_bytes))
```

For `SKILL.md`, the canonical input is:

```text
canonicalize(manifest_json) + "\n" + normalize_markdown(content_markdown)
```

Baseline normalization rules:

| Source | normalize_version | Rules |
| --- | --- | --- |
| `pg_dump` | `pg_dump_v1` | Remove dump timestamp, server version comments, absolute paths, session ids, and volatile ownership metadata. |
| `slow_query_analyzer` | `slow_query_v1` | Normalize timestamps to buckets, strip host/pid/session ids, preserve query text and plan shape. |
| `fetch_url` | `fetch_url_v1` | Strip fetch time, transient headers, tracking query params, and normalize whitespace. |
| `parse_pdf` | `parse_pdf_v1` | Normalize metadata order, page-break markers, and whitespace; preserve locator offsets. |
| `SKILL.md` | `skill_md_v1` | Sort YAML/manifest keys, normalize newlines to LF, trim trailing whitespace, and collapse repeated blank lines outside code fences. |
| raw bytes | `raw_v1` | Identity transform; preserve bytes exactly and hash `raw_bytes` directly. |

If a schema or normalization rule changes, `normalize_version` must bump.
Deduplication, vector stale detection, artifact reuse, and skill version audit
must compare both `content_hash` and `normalize_version`.

`normalize_version=NULL` is deprecated for new writes. Legacy NULL values are
read as `raw_v1` until migrated.

<!-- /GAP-FIX: G17 -->

### 6. Plans and Todos

Keep existing `plans`, `plan_step_runs`, `agent_tasks`, and task contracts.
Add a small todo table only if `agent_tasks` remains too heavyweight for
interactive checklist state:

```sql
CREATE TABLE IF NOT EXISTS session_todos (
  todo_id       VARCHAR(128) PRIMARY KEY,
  user_id       VARCHAR(128) NOT NULL,
  session_id    VARCHAR(128) NOT NULL,
  plan_id       VARCHAR(128) NULL,
  parent_id     VARCHAR(128) NULL,
  backlog_pool_id VARCHAR(128) NULL,
  origin_session_id VARCHAR(128) NULL,
  depth         INT NOT NULL DEFAULT 0,
  path          VARCHAR(2048) NULL,
  title         VARCHAR(255) NOT NULL,
  description   TEXT NULL,
  summary_text  TEXT NULL,
  status        VARCHAR(32) NOT NULL,
  priority      VARCHAR(32) NOT NULL DEFAULT 'medium',
  position      INT NOT NULL,
  source        VARCHAR(64) NOT NULL,
  provenance_event_id VARCHAR(128) NULL,
  request_id    VARCHAR(128) NULL,
  trace_id      VARCHAR(128) NULL,
  created_at    TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at    TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  INDEX idx_todos_session_position (session_id, position),
  INDEX idx_todos_session_status (session_id, status, position),
  INDEX idx_todos_plan (plan_id, position),
  INDEX idx_todos_parent_status (session_id, parent_id, status, position),
  INDEX idx_todos_backlog_pool (user_id, backlog_pool_id, status, updated_at)
);

CREATE TABLE IF NOT EXISTS session_todo_deps (
  todo_id       VARCHAR(128) NOT NULL,
  depends_on    VARCHAR(128) NOT NULL,
  user_id       VARCHAR(128) NOT NULL,
  session_id    VARCHAR(128) NOT NULL,
  created_at    TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY (todo_id, depends_on),
  INDEX idx_todo_deps_session (session_id)
);
```

The context builder should render only active/in-progress todos and immediate
dependencies. Completed todo history remains visible in UI/history but should
not consume prompt tokens unless relevant.

<!-- GAP-FIX: G5 -->

### Plan Tree Rendering Policy

For plan trees deeper than a flat checklist, prompt rendering is structural and
bounded. The context assembler must not serialize the whole transcript and ask
the model to infer the current plan.

Rules:

1. Always render the current path's ancestor chain with full title, status, and
   one-line summary.
2. Render all non-archived subtasks under the current section with title,
   status, priority, and one-line summary.
3. Render sibling sections as title + status only.
4. Cross-subtree inspection is an explicit API request, not a side effect of a
   normal turn: `GET /sessions/{id}/plan/subtree?root=...`.
5. Manifest reasons must distinguish `plan_subtree_query` and
   `tree_structured_report` so expensive plan renders are observable.

Ancestor-chain query:

```sql
WITH RECURSIVE ancestors AS (
  SELECT todo_id, parent_id, title, status, summary_text, depth, path
  FROM session_todos
  WHERE session_id = :session_id AND todo_id = :current_todo_id
  UNION ALL
  SELECT p.todo_id, p.parent_id, p.title, p.status, p.summary_text, p.depth, p.path
  FROM session_todos p
  JOIN ancestors a ON a.parent_id = p.todo_id
  WHERE p.session_id = :session_id
)
SELECT todo_id, title, status, summary_text, depth
FROM ancestors
ORDER BY depth ASC;
```

Current-section pending subtree query:

```sql
SELECT todo_id, parent_id, title, status, priority, summary_text, depth, position
FROM session_todos
WHERE session_id = :session_id
  AND parent_id = :section_todo_id
  AND status NOT IN ('archived', 'cancelled', 'done')
ORDER BY position ASC
LIMIT :budgeted_limit;
```

For very deep trees, `path` is a materialized path used for display and bounded
subtree scans; recursive CTE remains the correctness fallback.

<!-- /GAP-FIX: G5 -->

<!-- GAP-FIX: G6 -->

### Cross-Session Scope and User Memory

`session_state_items.scope` is an enum-like contract:

- `session`: visible only inside one session.
- `user`: durable memory promoted across sessions for the same user.
- `project`: shared by sessions with the same `project_id`.
- `workspace`: tied to a cloud/edge workspace identity.

For non-session scopes, `session_id` stores the namespace owner key rather than
the UI session id: `user:<user_id>`, `project:<project_id>`, or
`workspace:<workspace_id>`. The original UI session remains in
`origin_session_id`.

`scope='user'` is allowed only for these initial categories:

- `durable_decision`
- `engineering_rule`
- `rejected_pattern`

User-scope items must carry provenance in real columns:
`origin_session_id`, `origin_chunk_id`, or `origin_state_item_id`. The same refs
may also appear in `payload_json` for display, but queries must use columns.

New-session initialization loads user memories into the `session_anchor` zone:

```sql
SELECT item_id, category, item_key, title, summary_text, payload_hash, token_estimate
FROM session_state_items
WHERE user_id = :user_id
  AND scope = 'user'
  AND status = 'active'
  AND category IN ('durable_decision', 'engineering_rule', 'rejected_pattern')
ORDER BY priority DESC, updated_at DESC
LIMIT :budgeted_limit;
```

The default budget for user memory in a new session is `<=400` tokens. If more
items match, the assembler includes only the highest priority items and records
dropped refs in `context_manifest_items`.

Backlog todos that should survive a session reset use
`session_todos.backlog_pool_id` and `status='backlog'`. A new session can attach
the same backlog pool without copying todo payloads or losing provenance.

Cross-session history retrieval must include `user_id` and a bounded
`chunk_type` predicate. Missing `user_id` is an authorization error, not a broad
search fallback.

<!-- /GAP-FIX: G6 -->

## Context Assembly

### Per-Turn Flow

1. Authenticate `user_id` and authorize `session_id`.
2. Resolve or create `agent_sessions` row.
3. Load current run/session projection:
   - latest CSL materialization from `conversation_log`;
   - current `session_state_items`;
   - active plan/todos/tasks;
   - user preferences and personal skills;
   - recent artifacts/tool previews;
   - workspace and edge/cloud tool availability.
4. Generate candidate context items with:
   - `source_table`, `source_id`, `source_hash`;
   - token estimate;
   - freshness and priority;
   - category/zone;
   - render mode.
5. Allocate token budget by zones.
6. Render prompt messages and persist `context_manifests` plus
   `context_manifest_items`.
7. Call the LLM.
8. Persist run events, agent events, CSL deltas/snapshots, tool outputs, updated
   state projections, and final usage.

### Budget Policy v1

Budget should be model-aware and prompt-cache aware. A reasonable first policy:

| Zone | Target |
| --- | --- |
| Stable system prefix | Keep stable for cache; do not include volatile facts. |
| Tool schemas | Prune by active agent, permissions, skill/tool relevance. |
| Session anchor | Always include; keep under a hard cap. |
| Plan/todos | Always include active plan phase and next actionable items. |
| Delegation state | Include bounded child-agent summaries; never raw child transcripts. |
| Recent tail | Last 2-4 turns or budgeted recent messages. |
| Summary | Include latest L1b summary when tail is insufficient. |
| Retrieved facts | Top-K facts by category relevance and recency. |
| Tool previews | Include short previews only; link artifacts. |
| Safety/approvals | Include pending approvals and blocked tools. |

Default target:

- Reserve output tokens first.
- Reserve a safety buffer for tool calls.
- Keep stable prefix separate from volatile per-turn state.
- Prefer structured L1a facts over verbose older prose.
- Use L1b summaries for older history.
- Never include full raw outputs unless the current user explicitly asks to
  inspect that output and the budget allows it.

<!-- GAP-FIX: G10 -->

#### Small-Window Budget Template

Small-window models need a separate profile; do not scale the 200k-token policy
linearly. For models with `context_window_tokens <= 16000`, use
`budget_v1_8k` unless the deployment overrides it:

| Zone | Cap |
| --- | ---: |
| Session anchor | 200 |
| Plan/todos | 400 |
| Recent tail | 2000, temporarily expandable to 2800 |
| Summary | 500 |
| Retrieved facts | 1000 |
| Tool previews | 500 |
| System + tool schemas | 3400 |
| Reserved output | 500 |
| Safety buffer | 200 |

Rules:

- Tool schemas and retrieved facts are aggressively pruned first.
- Recent tail has a floor of 1600 unless the turn is an explicit retrieval or
  blocker review.
- Vector retrieval is disabled by default in `budget_v1_8k`; it may be enabled
  only when structured and FTS both fail and at least 1000 retrieved-fact tokens
  remain.
- `context_manifests` must record `tokenizer_id` and
  `budget_template_id='budget_v1_8k'` because token estimates can drift by 15%
  across model families.

<!-- /GAP-FIX: G10 -->

<!-- GAP-FIX: G18 -->

#### Delegation State Budget

`delegation_state` is a first-class context zone for parent sessions. It has a
hard total cap of `1500` tokens before explicit user drill-down. The intended
formula is capped division with a floor.

DEPRECATED in v0.3: the old v0.2 formula
`min(1200, max(200, floor(1500 / active_children)))` is invalid when
`active_children >= 8` because the 200-token floor can exceed the 1500-token
zone cap.

<!-- GAP-FIX: G21 -->

The v0.3 formula separates **candidate children** from **rendered children**:

```text
budget_total = 1500
min_child_floor = 200
max_rendered_children = floor(budget_total / min_child_floor)  -- 7

candidate_children = active non-terminal children ordered by:
  blocker severity DESC,
  status priority (blocked > running > waiting > completed),
  updated_at DESC,
  priority DESC

rendered_children = first max_rendered_children from candidate_children
overflow_children = remaining candidate_children

per_child_budget = max(200, floor(budget_total / count(rendered_children)))
```

If `active_children=0`, the zone is omitted. If there are overflow children,
the prompt renders one compressed overflow row such as
`"3 more children not shown; 1 blocked; open delegation tree for details"` and
records each omitted child in `context_manifest_items(included=false,
reason='delegation_child_overflow')`.

Boundary verification:

| active children | rendered children | per child | rendered total | behavior |
| ---: | ---: | ---: | ---: | --- |
| 1 | 1 | 1500 | 1500 | one child may use the full zone |
| 3 | 3 | 500 | 1500 | all rendered |
| 5 | 5 | 300 | 1500 | all rendered |
| 7 | 7 | 214 | 1498 | all rendered, 2-token slack |
| 8 | 7 | 214 | 1498 | 1 overflow child summarized |
| 10 | 7 | 214 | 1498 | 3 overflow children summarized |
| 15 | 7 | 214 | 1498 | 8 overflow children summarized |

The hard zone cap is never relaxed for normal parent prompts. Explicit
drill-down APIs are the escape hatch for overflow children, not hidden prompt
expansion.

<!-- /GAP-FIX: G21 -->

Assembly rules:

- Pre-check `session_delegations.last_summary_token_estimate` before loading a
  child summary.
- If one child summary exceeds `per_child_budget`, render
  `title + phase + status + blocker + artifact_ref` and record the summary as
  dropped in `context_manifest_items`.
- If a child has an active blocker, that child may temporarily use
  up to `2 * per_child_budget` by borrowing budget from lower-priority rendered
  children first, then from `recent_tail`. The final `delegation_state` zone
  still stays under `budget_total=1500`; if there is not enough budget, the
  blocker child gets priority and lower-priority children move to overflow. The
  manifest records `reason='partial_blocker_review'`.
- Parent context never expands child raw transcript. Exact child details require
  `GET /chat/runs/{run_id}/delegation-summary` or child-session drill-down.

<!-- /GAP-FIX: G18 -->

### Compaction

Compaction must update projections, not just write a hidden summary.

When compaction runs:

1. Keep full history in `conversation_log` and `agent_events`.
2. Write a compact summary row into `conversation_log` and/or
   `session_state_items(category='summary')`.
3. Preserve structured L1a facts: files, decisions, active errors, plan/todos,
   current constraints, blocked tools.
4. Mark old tool outputs as compacted and keep artifact refs.
5. Persist a `context_manifest` for the compaction turn.
6. On the next normal turn, include the summary plus recent tail, not all old
   turns.

For compaction outputs, `session_state_items(category='summary')` is the
authority for context assembly. Any `conversation_log` summary row is a
narrative replay/display aid and must reference the authoritative summary item.

This follows the existing session-memory protocol and avoids the common failure
where a compacted session forgets the actual task state.

<!-- GAP-FIX: G2 -->

#### Compaction Invariants

Compaction is a projection job, not a destructive rewrite. The raw audit chain
remains append-only, and the active structured state has stricter rules than
the narrative summary.

Required invariants:

1. Raw `conversation_log`, `agent_events`, `agent_run_events`, and history chunk
   rows are never physically rewritten by compaction.
2. Active L1a state in categories `plan_state`, `decision`, `finding`,
   `benchmark`, `citation`, active `todo_state`, active `error_state`, and
   active `delegation_state` must not be replaced or archived by compaction.
3. `plan_state.version` must not bump during a compaction turn. A compactor may
   add summary references, but it cannot imply a semantic plan edit.
4. A session-level compaction trigger must first verify no run in that session
   is `running` or `waiting`. Run-local compaction is allowed only over already
   closed event ranges.
5. Completed subtasks may be archived to reduce prompt pressure, but their
   provenance event, completion evidence, and parent plan edge remain queryable.
6. Every compaction writes a `context_manifest` with
   `reason='post_compaction'`, token totals, included summary refs, and dropped
   refs.
7. First-class `finding`, `benchmark`, and `citation` items keep their own state
   rows; they are not only embedded inside a prose summary.
8. New summaries are appended as new `summary` items or summary versions. Older
   summaries can be marked archived, but not deleted.

The compactor must run these assertions after each job. `:compaction_event_id`
is the provenance id written by the compactor.

```sql
-- 1 + 2. No destructive mutation of active high-value L1a state.
SELECT COUNT(*) AS forbidden_state_mutations
FROM session_state_item_events e
JOIN session_state_items i ON i.item_id = e.item_id
WHERE e.session_id = :session_id
  AND e.provenance_event_id = :compaction_event_id
  AND e.mutation IN ('replace', 'archive', 'delete')
  AND (
    i.category IN ('plan_state', 'decision', 'finding', 'benchmark', 'citation')
    OR (i.category IN ('todo_state', 'error_state', 'delegation_state')
        AND i.status IN ('active', 'in_progress', 'blocked', 'waiting'))
  );

-- 3. Compaction cannot bump the semantic plan version.
SELECT COUNT(*) AS forbidden_plan_version_bumps
FROM session_state_item_events
WHERE session_id = :session_id
  AND provenance_event_id = :compaction_event_id
  AND category = 'plan_state'
  AND next_version IS NOT NULL
  AND previous_version IS NOT NULL
  AND next_version <> previous_version;

-- 4. Session-level compaction cannot run while a run is active.
SELECT COUNT(*) AS active_runs
FROM agent_runs
WHERE session_id = :session_id
  AND status IN ('running', 'waiting');

-- 5. Archived completed todos must retain provenance.
SELECT COUNT(*) AS archived_todos_without_provenance
FROM session_state_items
WHERE session_id = :session_id
  AND category = 'todo_state'
  AND status = 'archived'
  AND provenance_event_id IS NULL;

-- 6. The compaction turn must have an explicit manifest.
-- Note: session-level compaction jobs may run without an owning run; in that
-- case :compaction_run_id is NULL and context_manifests.run_id is also NULL.
SELECT COUNT(*) AS post_compaction_manifest_count
FROM context_manifests
WHERE session_id = :session_id
  AND ((run_id = :compaction_run_id) OR (run_id IS NULL AND :compaction_run_id IS NULL))
  AND reason = 'post_compaction';

-- 7. Durable fact categories must have standalone provenance.
SELECT COUNT(*) AS durable_facts_without_provenance
FROM session_state_items
WHERE session_id = :session_id
  AND category IN ('finding', 'benchmark', 'citation')
  AND status = 'active'
  AND provenance_event_id IS NULL;

-- 8. Summary rows are appended or archived, not deleted.
SELECT COUNT(*) AS deleted_summaries
FROM session_state_item_events
WHERE session_id = :session_id
  AND provenance_event_id = :compaction_event_id
  AND category = 'summary'
  AND mutation = 'delete';
```

All count queries above must return `0`, except
`post_compaction_manifest_count`, which must return `1`.

<!-- /GAP-FIX: G2 -->

### Web Resume vs LLM Resume

These are different APIs:

- Web display resume loads a human/UI projection: transcript, plan, todos,
  artifacts, runs, latest context explanation.
- LLM resume builds a fresh runtime context manifest from durable state.

The browser should not upload its local transcript back to the server as source
of truth. It should send `session_id`, user input, and any browser-only
attachments; the server reconstructs context from MatrixOne.

## Web Agent Interaction Design

### Product Surface

The current workspace layout is a good base. Keep it dense and operational:

- Left: sessions, filters, active/running/waiting status, search.
- Center: transcript with message parts, tool calls, compacted summaries,
  artifacts, errors, approvals.
- Right tabs:
  - `Plan`: active plan, todos, dependencies, progress, blockers.
  - `Tools`: live and historical tool timeline.
  - `Context`: latest context manifest, token usage by zone, dropped items.
  - `Files`: cloud workspace artifacts, diffs, generated files.
  - `Runs`: run status, retries, child agents, reconnect/cancel.
  - `Skills`: active/personal skills used in this session.

No marketing/landing surface is needed for the workspace. The first viewport
should be the usable agent console.

### Interaction Semantics

- Opening an existing session calls `GET /sessions/{id}/state`.
- Transcript is loaded by cursor from `conversation_log` or a display projection:
  `GET /sessions/{id}/transcript?limit=100&before=...`.
- If a run is active, the client reconnects to
  `GET /chat/runs/{run_id}/stream?last_index=...`.
- Stop button calls backend cancel, not only local SSE close.
- Approval/question prompts must be durable run events. Browser answers through
  `POST /chat/runs/{run_id}/input`.
- Approval through an external approver is not execution approval by itself.
  Approved runs move to `pending_requester_confirm` until the requester sends a
  durable `requester_confirm` input.
- `waiting_for_edge` has a default 300s timeout and writes `edge_timeout` before
  transitioning according to the run policy.
- The context side panel reads `GET /sessions/{id}/context/latest` and never
  recomputes context in the browser.
- A session can be in one of these visible states:
  `idle`, `running`, `waiting_for_user`, `waiting_for_edge`,
  `waiting_for_external`, `failed`, `cancelled`, `completed`.

### Cloud Workspace vs Edge Workspace

Each session needs a workspace authority:

- `cloud`: server sandbox/workspace is authoritative; web tools operate there.
- `edge`: user device or edge executor is authoritative; cloud stores session
  state and routes tool calls through the edge bridge.
- `hybrid`: cloud can run safe read-only or remote APIs, edge owns local file
  mutation.

Persist this in `session_state_items(category='workspace_state')` and expose it
in UI. The agent context must know which tools are available and where files
actually live.

<!-- GAP-FIX: G11 -->

### Workspace Reachability and Degradation Semantics

`workspace_state.payload_json` must include a reachability projection:

```json
{
  "authority": "hybrid",
  "workspace_id": "ws_...",
  "edge_bridge_id": "edge:vpc-fin-prod",
  "edge_status": "online",
  "tool_whitelist": ["sql.explain", "sql.dry_run", "sql.execute"],
  "reachability_probe": {
    "last_ok_at": "2026-05-07T10:00:00Z",
    "last_fail_at": null,
    "probe_method": "HEAD",
    "rtt_ms": 42
  }
}
```

Reachability states:

- `online`: edge probe succeeded; edge tools may be selected.
- `reconnecting`: transient failure within grace window; edge tools are hidden
  from new LLM turns but UI can show reconnecting.
- `offline`: probe failed or timed out; authority projection degrades to cloud
  or read-only hybrid according to policy.
- `detached`: user/admin explicitly detached the edge bridge; no automatic
  reattach without identity reconciliation.

Cloud relay rule: before forwarding an edge tool call, run a 200ms reachability
probe. On timeout or failure, write a workspace-state projection update, emit
`workspace_reachability_changed`, and rebuild tool schemas without edge-only
tools. The browser must not make this decision locally.

Reattach protocol:

- Edge bridge presents stable `edge_bridge_id`, `device_fingerprint`, and
  user auth.
- Server compares against the latest `workspace_state` and
  `session_device_leases`.
- If identity matches, status moves to `online`; otherwise it remains
  `detached` and requires user confirmation.

Minimum API:

- `POST /edge/bridges/{id}/detach`
- `POST /edge/bridges/{id}/reattach`

<!-- /GAP-FIX: G11 -->

## API Additions

Recommended v1 endpoints:

```text
GET  /sessions/{session_id}/state
GET  /sessions/{session_id}/transcript?limit=&before=
GET  /sessions/{session_id}/context/latest
GET  /sessions/{session_id}/context/{manifest_id}
POST /sessions/{session_id}/context/rebuild?dry_run=true
GET  /sessions/{session_id}/plan/subtree?root=
GET  /sessions/{session_id}/delegations?root_run_id=
GET  /sessions/{session_id}/devices
POST /sessions/{session_id}/device/revoke

GET  /chat/runs/{run_id}
GET  /chat/runs/{run_id}/stream?last_index=
POST /chat/runs/{run_id}/cancel
POST /chat/runs/{run_id}/input
GET  /chat/runs/{run_id}/children
GET  /chat/runs/{run_id}/delegation-summary

GET  /artifacts/{artifact_id}
GET  /artifacts/{artifact_id}/download-url
GET  /artifacts/{artifact_id}/grants
POST /artifacts/{artifact_id}/grants

GET  /preview-templates
GET  /preview-templates/{tool_name}/{version}
GET  /tool-executors
GET  /raw-ref-schemes

POST /notifications/adapters/{adapter}/callbacks

POST /edge/bridges/{bridge_id}/detach
POST /edge/bridges/{bridge_id}/reattach

GET  /skills/user
POST /skills/user
GET  /skills/user/{skill_name}/versions
POST /skills/user/{skill_name}/versions
POST /skills/user/{skill_name}/activate
POST /skills/user/{skill_name}/install
GET  /skills/user/{skill_name}/evaluations
POST /skills/user/{skill_name}/evaluations
```

`POST /chat/runs/{run_id}/input` requires an `idempotency_key` in the request
body or `Idempotency-Key` header.

`/sessions/{id}/state` should return one display projection:

```json
{
  "session": {},
  "workspace": {},
  "active_run": {},
  "plan": {},
  "todos": [],
  "messages_cursor": null,
  "recent_messages": [],
  "artifacts": [],
  "skills": [],
  "latest_context_manifest": {}
}
```

This projection is for UI hydration. It is not the LLM prompt.

## Implementation Plan

### Phase 1: Make Run Durability Real

- Add MatrixOne schema for `agent_runs`, `run_counters`, and
  `agent_run_events`.
- Implement `DatabaseRunStateStore` in `rust/crates/services/src/runs.rs`.
- Implement run owner lease, counter allocation, checkpoint_v1, and
  idempotency-key dedupe for run input.
- Wire `state_builder.rs` to use DB store when a shared MatrixOne pool exists.
- Update run stream/cancel/reconnect tests to prove DB replay after in-memory
  cache loss.

Exit criteria:

- Restarting the API server preserves run status and streamable historical run
  events.
- `/chat/runs/{run_id}/stream?last_index=N` works across workers.
- Cancel writes a durable terminal state.
- Graceful shutdown writes `checkpoint_json.graceful=true`; the next pod resumes
  and emits `run_resumed_after_restart`.
- Crash without a graceful checkpoint does not continue the same execution; it
  marks the run failed or starts an explicit retry run.
- Duplicate `/chat/runs/{run_id}/input` requests with the same
  `idempotency_key` produce one state transition.
- SSE heartbeat follows the 15s server / 45s client timeout contract.

### Phase 2: Web Transcript Hydration

- Add transcript projection from `conversation_log` materialization and/or
  `agent_events`.
- Add `/sessions/{id}/state` and `/sessions/{id}/transcript`.
- Update `web/hooks/use-chat-stream.ts` and workspace components so selecting a
  session hydrates existing messages instead of clearing to empty state.
- Stop button calls backend cancel.
- Add Web cache watermarks: `state_revision`, `transcript_high_watermark`,
  `run_event_high_watermark`, and page hashes.
- Add client IndexedDB cache for transcript pages and session display state.
- Add session device leases, revision reconciliation, cold-start hydration,
  lease-end SSE parity, and IndexedDB watermark atomicity.

Exit criteria:

- Create a session in browser A, continue it in browser B, and see the same
  transcript/plan/tool state.
- Refresh during a run and reconnect without losing streamed events.
- Reopening a session with a warm Web cache fetches only deltas.
- IndexedDB event rows and `run_event_high_watermark` commit atomically; gap
  detection replays from `last_ok_idx`.
- Revoked or stale devices cannot move a session revision backward.
- Cold-start clients with empty cache hydrate transcript/run events from replay
  instead of treating server watermarks as locally applied.
- Passive lease expiry emits the same local-cache clearing signal as explicit
  device revoke.

### Phase 3: Context Manifest v1

- Add `context_manifests` and `context_manifest_items`.
- Add `context_manifest_reason_types` and validate reason enum in Rust before
  manifest writes.
- Wrap the existing context assembly path so every LLM call writes a manifest.
- Render a context side-panel from the manifest.
- Track dropped items and token budgets by zone.
- Implement retrieval state machine events and delegation-state budget
  allocation.
- Implement `budget_v1_8k` and next-action confidence thresholds.
- Add Sprint D manifest reasons, `turn_intent`, intent-aware budget overrides,
  and the corrected fan-out-safe delegation budget formula.

Exit criteria:

- Every web turn has a queryable manifest.
- Every manifest reason is one of the seeded enum values.
- A failing/expensive turn can be explained by inspecting included/dropped
  sources.
- Retrieval stages respect structured/FTS/vector SLA and emit degradation
  events on timeout, empty result, or stale vector hash.
- `delegation_state` zone never exceeds its cap and records dropped child
  summaries.
- Fan-out budget property tests pass for active child counts 1, 3, 5, 7, 8, 10,
  and 15.
- Small-window manifests record `tokenizer_id` and `budget_template_id`.
- Unknown manifest reasons fall back to `other` only with an alert event and
  `reason_original` audit data.
- Ambiguous "continue" turns produce bounded suggestions and ask-user prompts
  according to thresholds.
- No production query filters on JSON payloads.

### Phase 4: State Projection v1

- Add `session_state_items` and `session_state_item_events`.
- Populate initial categories: `anchor`, `summary`, `plan_state`,
  `todo_state`, `active_file`, `tool_ref`, `error_state`, `workspace_state`,
  `delegation_state`, `finding`, `benchmark`, `citation`, `active_skill`,
  `durable_decision`, `engineering_rule`, and `rejected_pattern`.
- Add `session_delegations` and keep it synchronized with child run creation,
  status changes, and child summary updates.
- Add approval condition and external notification projections.
- Add plan tree rendering policy, backlog pools, and user-scope state loading.
- Add workspace reachability/degradation projection and delegation retry/bubble
  contracts.
- Add same-root-tree artifact grants and retry-scope selection rules.
- Update compaction and post-turn persistence to maintain projections.
- Add cheap next-action extraction:
  - structured event first;
  - rule extraction second;
  - small model only when needed.

Exit criteria:

- New turns can build context from projection + recent tail instead of scanning
  broad event history.
- Compaction preserves structured plan/todo/error/file facts.
- Compaction SQL assertions pass, including no active-run compaction and
  required `reason='post_compaction'` manifest.
- Delegation tree UI can render child runs and optional child sessions without
  reading raw child transcripts.
- Approval waits, condition changes, requester confirmation, and notification
  delivery are durable and queryable without JSON filtering.
- Cross-session user memory loads into new sessions within the configured
  anchor budget.
- Edge tools disappear from manifest within one failed reachability probe.
- Delegation retries mark old branches `superseded` and keep drill-down audit.
- Sibling child agents can read explicitly exposed artifacts under the same
  root run, while different roots remain denied.
- Retry runs carry `retry_scope` through suggestion application, run creation,
  and audit events.
- "Continue" works for paused tasks and recent suggested next actions without
  scanning the full transcript.

### Phase 5: Personal Skills in Web

- Add user skill source/version tables or equivalent extensions to
  `skills_registry`.
- Add `skill_installations.scope`, active skill session state, and
  `user_skill_evaluations`.
- Build skill CRUD/version/activate/install APIs.
- Integrate personal skills and API-server HOME skills into the shared skill
  selector and context manifest surface.
- Add privacy/ownership checks and tests.

Exit criteria:

- User can create a private skill in web, use it in a web session, switch
  devices, and keep the skill/session behavior.
- Installed skills do not enter prompts until session activation or configured
  auto-activation.
- Skill quality gates can query `user_skill_evaluations` without JSON filters.

### Phase 6: Artifact and Tool Output Policy

- Store large tool outputs as artifacts/tool output rows.
- Store fan-out tool outputs through batch insert contracts.
- Add preview generation and artifact references to context manifest.
- Add preview template registry and enforce per-tool preview contracts.
- Add content hash normalization contracts for tools and skills.
- Add tool executor registration, raw_ref scheme registry, and expanded baseline
  templates.
- Add retention and deletion policy.
- Add artifact reference counters, access scopes, project retention policy, and
  expired-artifact degradation.
- Add lazy history chunk/index jobs for large outputs and old transcript slices.

Exit criteria:

- 1000 `session_tool_outputs` rows insert in `<500ms` through the batch path on
  the supported MatrixOne deployment, excluding raw artifact upload time.
- Tool executors cannot write production outputs without a registered preview
  template and `normalize_version`.
- `raw_ref` strings parse through the canonical scheme registry and run the same
  artifact ACL check as direct download APIs.
- Rust toolchain, SQL compatibility, slow query, PDF, URL, file, eslint, and
  skill-diff baseline templates are seeded.
- Add retrieval tiers: structured filters, full-text, then vector only when
  needed.

Exit criteria:

- Large tool output does not bloat future prompts.
- UI can still inspect full outputs/artifacts.
- Long-session old-detail lookup returns raw referenced slices without scanning
  the full session.

## Testing Strategy

Follow the project rules: tests are isolated, parallel-safe, and DB E2E tests
assert state directly.

Required test groups:

- Unit: `DatabaseRunStateStore` insert/update/list/replay/cancel/retry.
- Unit: context budget allocator with deterministic candidate sets.
- Unit: state projection updates for plan/todo/error/tool cases.
- Integration: `/chat/stream` creates session, run, run events, CSL entries,
  context manifest, and state items.
- Integration: API restart or new `RunEngine` instance can replay run stream.
- Integration: two users cannot access each other's session, manifests, skills,
  or artifacts.
- Integration: personal skill create/activate/select/use.
- E2E web: create session, refresh, resume, reconnect run, cancel run.
- E2E DB: after mutation, SELECT `conversation_log`, `agent_runs`,
  `agent_run_events`, `context_manifests`, and `session_state_items` directly.
- Performance: context manifest assembly queries hit indexes and do not scan JSON
  payload columns.

## Design Decisions

1. MatrixOne is the source of truth for web sessions. Local CLI state is a peer
   cache/sync source, not authoritative for web.
2. `conversation_log` remains the canonical conversational continuity store.
   Add projections rather than replacing it.
3. `agent_events` remains the audit/event trail. Do not duplicate every event
   into `session_state_items`; only project current useful state.
4. The LLM prompt is always rebuilt from server-side durable state and a
   persisted context manifest.
5. Plans/todos must be visible in UI and included in context as structured
   state, not only embedded in natural-language transcript.
6. Personal skills are versioned and user-owned. Activation changes pointers;
   historical content remains immutable.
7. Large tool outputs are stored for audit/display and represented in prompt by
   previews/references.
8. Run durability is a prerequisite for production web agent. It should be done
   before polishing web UI.

## Open Questions

- Cloud workspace lifecycle: per-session container, shared user workspace, or
  pooled sandbox? This affects artifact retention and file permissions.
- Retention policy: how long to store full transcripts, tool outputs, and
  artifacts by default? What does user deletion mean across logs, indexes, and
  derived memories?
- Context retrieval: should MatrixOne vector/full-text retrieval be part of v1,
  or should v1 use only structured projection + recent tail?
- Plan/todo unification: should interactive todos be a thin view over
  `agent_tasks`, or a separate `session_todos` table synchronized to tasks when
  needed?
- Skill packaging: should personal skills be edited as raw `SKILL.md`, guided
  form fields, or both?

Resolved in v0.2:

- Multi-agent web UX uses both forms. `agent_runs.parent_run_id` is the
  mandatory execution tree; child `agent_sessions` are created only when a child
  agent needs independent identity.

## Near-Term Recommendation

Do not start by redesigning the whole session model. Start with the two missing
contracts that web agent production needs:

1. **Durable run store**: implement DB-backed run status/events and wire it into
   the server.
2. **Context manifest**: persist exactly what every LLM turn sees.

After that, add transcript hydration and state projections. This order makes
subsequent UI and context work debuggable instead of speculative.

## Changelog v0.2

- Resolved G15 by specifying DB-backed run event ordering with `run_counters`,
  run owner leases, checkpoint_v1, idempotent run input, restart semantics, and
  SSE heartbeat/multi-tab replay.
- Resolved G4 by making the multi-agent model explicit: run parent/child edges
  are mandatory, child sessions are optional identities, and
  `session_delegations` is the queryable delegation projection.
- Resolved G2 by adding compaction invariants, SQL assertions, durable fact
  categories, and the requirement that every compaction writes a
  `reason='post_compaction'` manifest.
- Resolved Sprint B G1/G3/G18 by adding manifest reason enum, retrieval SLA and
  degradation events, and a bounded `delegation_state` budget formula.
- Resolved Sprint B G5/G6 by adding plan tree rendering policy, backlog pools,
  user-scope state categories, provenance columns, and cross-session retrieval
  indexes.
- Resolved Sprint B G7 by adding approval conditions, notification delivery
  projections, requester confirmation semantics, server-time expiry, and edge
  timeout policy.
- Resolved Sprint B G8/G9 by adding preview template registry, first-class
  preview columns, structured `finding`/`benchmark`/`citation` payloads,
  artifact retention counters, access scopes, and expired-artifact degradation.
- Resolved Sprint C G10/G12 by adding `budget_v1_8k`, tokenizer/budget fields,
  and next-action confidence thresholds.
- Resolved Sprint C G11/G13/G19 by adding workspace reachability probes,
  device leases, revision reconciliation, and Web cache watermark atomicity.
- Resolved Sprint C G14 by adding `retry_scope`, `superseded`, mutation enums,
  bubble-up/apply-suggestion contracts, and delegation drill-down mode.
- Resolved Sprint C G16/G17 by adding per-session skill activation,
  `user_skill_evaluations`, skill installation scope, and content hash
  normalization contracts.

## Changelog v0.3

- Resolved Sprint D G20 by replacing ambiguous delegation artifact access with
  same-root-tree ACL, explicit sibling grants, owner/root run indexes, and an
  access matrix.
- Resolved Sprint D G21 by correcting the delegation fan-out budget formula,
  adding overflow summarization, and documenting the 1/3/5/7/8/10/15 boundary
  table.
- Resolved Sprint D G22 by adding retry-scope selection rules and propagating
  `retry_scope` through `apply_suggestion`, retry run creation, and audit
  events.
- Resolved Sprint D G23 by adding `session_tool_output_batches`, batch columns,
  and the 1000-row `<500ms` batch insert contract for fan-out tools.
- Resolved Sprint D G24/G25 by adding cold-start hydration replay flags and
  SSE parity for passive device lease expiry versus explicit revoke.
- Resolved Sprint D G26 by adding manifest reasons for ambiguity, user memory,
  progressive loading, and intent-driven preview expansion, plus
  `turn_intent` and unknown-reason fallback.
- Resolved Sprint D G27 by adding tool executor registration, canonical raw_ref
  schemes, expanded baseline preview/normalize templates, `raw_v1`, and
  multi-source artifact provenance.

## Changelog v0.4

- Unified CLI/Web tool and skill visibility behind the capability catalog:
  Web and remote CLI use server-executable capabilities; local CLI uses
  client-executable capabilities plus the authenticated server-visible skill
  catalog. This codifies the API-server HOME skill boundary, DB skill visibility
  predicate, MCP execution-site boundary, and project-local CLI-only rule.
- Added the generic Web artifact publishing path for agent-generated files:
  `publish_artifact` is the only server-executable publication capability. The
  model can create charts/images/documents with existing execution tools, then
  publish the generated file from the session workspace or `/tmp` into
  `session_artifacts`; Web attaches newly created artifacts to the assistant
  message, previews supported image/text payloads inline, and offers download.
  Chat rendering treats `session_artifacts` as a mixed storage table: only
  `source='publish_artifact'` records with
  `metadata.normalize_version='artifact_file_v1'` are user-visible attachments.
  Internal artifacts such as `source='composite_snapshot_index'` remain
  queryable for restore/debug and must not be rendered in the transcript.
- Tightened the Web/server tool capability contract: `run_script` is now a real
  server-executable tool, not merely a schema-advertised capability. Its
  Python-side RPC allowlist is the server-routable subset
  (`read_file`, `write_file`, `list_dir`, `grep`, `web_fetch`, `bash`), so the
  LLM does not see `run_script` helper tools that the API server cannot
  execute. Each server tool round now batch-persists the model-visible tool
  result rows into `session_tool_outputs`, and each LLM manifest records a
  non-zero `system_tool_schemas` token estimate when tool schemas were exposed.
- Split resource governance into session-create quota and run-start quota.
  `max_sessions_per_day` is checked and incremented only around actual
  `agent_sessions` creation. Each chat turn may create a durable run, but run
  start checks only execution capacity (`max_concurrent_sessions`) and token
  budget; it must never increment `resource_usage.sessions_created`.
- Bound Web composer model selection to the session/chat. New chats may use
  the user's global default model as the initial value, but once a chat exists,
  model changes are persisted on that chat and mirrored to the remote
  `agent_sessions.metadata.current_model` field so switching between chats
  cannot leak one chat's model into another.
- Tightened Bedrock thinking request construction: unsigned historical
  `reasoning_content` is never serialized as Bedrock `reasoningContent`.
  Bedrock only accepts signed provider reasoning blocks, so mixed-provider or
  switched-model sessions keep visible text/tool calls while omitting invalid
  thinking blocks from the request body.
