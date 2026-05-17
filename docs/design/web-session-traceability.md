# Web Session Traceability

> Status: Design contract.
> Scope: Make every Web UI user input explainable from database-backed trace data, including parent turns, LLM rounds, tool calls, dynamic subagents, and final synthesis.
> Audience: Runtime, Web UI, harness, persistence, and observability maintainers.

This document defines the target design for answering one product/debugging
question:

```text
Given one Web chat input, what exactly did Astra do afterwards?
```

The answer must be reconstructable from database tables through a runtime API.
For Web agent deployments, database persistence is mandatory. Local JSONL
journals remain useful for developer debugging, but they are not durable product
state and must not be required for Web session recovery or trace rendering.

## Problem

Today Web chat has three related but non-equivalent persistence surfaces:

- `~/.astra/sessions/<session_id>.jsonl`: local append-only journal. This is the
  most complete trace today.
- `session_transcript_items`: durable conversation transcript used by session
  history and search.
- `agent_events`: structured DB events used by audit, sync, introspection, and
  analytics.

This is especially important for Web deployments. Web agents may run inside
Docker containers, remote sandboxes, or short-lived workers where local
filesystem state can disappear at restart, redeploy, eviction, or job
completion. A trace that exists only in `~/.astra` is therefore not a reliable
Web product capability.

For a Web UI session that triggers dynamic multi-agent work, the local JSONL
can show the complete chain:

```text
user input
  -> parent LLM round
  -> agent.spawn tool calls
  -> agent_spawned child lifecycle events
  -> child LLM rounds
  -> agent_terminated child lifecycle events
  -> agent.get_result tool calls
  -> final assistant synthesis
```

The database currently records only part of that story. In the observed
session `fc999d7d-adf4-4670-b43b-946d28f0c026`, `agent_events` recorded child
`user_query` and `llm_response` rows, but did not record explicit
`agent_spawned` or `agent_terminated` lifecycle rows. Tool calls were also
coarse aggregate rows such as `server-loop tool: agent`, without full
args/result lineage.

That means Web UI cannot reliably build a complete "what happened after this
input" timeline from DB alone.

## Goals

- Make DB the durable source of truth for Web session trace state.
- Make DB-backed trace reconstruction complete enough for Web UI and harness.
- Preserve local JSONL only as a diagnostic mirror, not as required Web state.
- Avoid parsing local JSONL in Web UI.
- Avoid creating parallel persistence logic for transcript, events, and JSONL.
- Make one user input traceable through parent run, child runs, LLM rounds,
  tool calls, lifecycle events, and final response.
- Keep event lineage explicit through stable IDs, not string matching.
- Support dynamic multi-agent fan-out as a first-class trace shape.
- Make tests fail if future changes drop trace links or event detail.

## Non-Goals

- Do not replace `session_transcript_items` as the conversation history table.
- Do not put all debug payloads into transcript rows.
- Do not make Web UI read files from `~/.astra`.
- Do not rely on container-local files for Web session durability.
- Do not require child agents to appear as separate Web chat sessions.
- Do not store secrets or full sensitive tool outputs in DB without the
  existing redaction policy.
- Do not make `agent_events` a byte-for-byte copy of local JSONL.

## Core Principle

For Web/server, `agent_events` is the durable trace fact table. JSONL is a
local diagnostic mirror of the same trace contract, plus optional local-only
debug detail. If DB and JSONL disagree for a Web session, DB is the product
source of truth and the disagreement is a trace health bug to investigate.

The split is:

```text
session_transcript_items
  Conversation transcript:
  user-visible messages, assistant final responses, session history/search.

agent_events
  Queryable execution trace:
  lifecycle, LLM rounds, tool calls, subagent lineage, errors, usage, timing.

local JSONL
  Local diagnostic mirror:
  same trace events where possible, plus local-only recovery/debug artifacts.
  Not required for Web session recovery.
```

If a Web UI feature needs to explain runtime behavior, it should query runtime
trace APIs backed by `agent_events`, not transcript rows or JSONL files.

## Current State

### Local Journal

`astra_services::session_journal` writes one JSON object per line to:

```text
~/.astra/sessions/<session_id>.jsonl
```

It already has event types such as:

- `llm_round`
- `agent_spawned`
- `agent_terminated`
- `pipeline_feedback`
- `turn_evaluation`

It also preserves detailed tool call records in `llm_round.tool_calls` for
many runtime paths.

### Transcript Tables

`session_transcript_items` stores user/assistant transcript rows:

- `session_id`
- `item_seq`
- `user_id`
- `run_id`
- `role`
- `content`
- `source_event_id`
- `content_hash`
- `created_at`

This is the right table for "what did the user and assistant say?", not for
"what did the runtime do?".

`session_history_chunks` stores derived searchable chunks over transcript or
history sources. It should not become the trace source of truth.

### Event Table

`agent_events` already exists and is the right persistence target for queryable
trace events. Relevant existing columns include:

- `event_id`
- `session_id`
- `user_id`
- `agent_id`
- `event_type`
- `content`
- `parent_event_id`
- `causal_chain_id`
- `token_usage`
- `llm_model_used`
- `metadata`
- `reasoning_content`
- `token_input`
- `token_output`
- `token_total`
- `meta_tool_name`
- `meta_duration_ms`
- `created_at`

Current gaps:

- No first-class `run_id` column.
- No first-class `parent_run_id` column.
- No first-class `turn_id` or `turn_seq` column.
- No first-class `round_index` column.
- No first-class `tool_call_id` column.
- Dynamic spawn lifecycle events are not consistently ingested.
- Server-loop tool rows are coarse and may lose args/result linkage.

## Target Trace Model

Every user input starts a trace root.

Required trace identifiers:

- `session_id`: durable Web session.
- `turn_id`: stable ID for one user input and all work caused by it.
- `turn_seq`: monotonic user-visible turn number in a session.
- `root_event_id`: event ID of the user input root event.
- `causal_chain_id`: stable chain ID shared by all events caused by this input.
- `run_id`: execution run that emitted the event.
- `parent_run_id`: parent run for child/subagent work.
- `agent_id`: logical agent identity for the event.
- `parent_agent_id`: parent agent identity for child/subagent work.
- `round_index`: zero-based LLM round inside a run.
- `tool_call_id`: model/tool call ID when the event is tied to a tool call.

The minimum reconstructable tree:

```text
TurnTrace
  user_query event
  parent RunTrace
    LlmRoundTrace round=0
      ToolCallTrace agent.spawn call=A
        child RunTrace
          agent_spawned
          child llm_round
          child llm_response
          agent_terminated
      ToolCallTrace agent.spawn call=B
        child RunTrace
    LlmRoundTrace round=1
      ToolCallTrace agent.get_result child=A
      ToolCallTrace agent.get_result child=B
    final llm_response
```

## Event Taxonomy

The trace contract should use these event types in `agent_events`.

### Core Turn Events

- `user_query`
- `llm_response`
- `turn_started`
- `turn_completed`
- `turn_interrupted`
- `turn_failed`

`user_query` is the root event for a user input. The final parent
`llm_response` should link to the root through `parent_event_id` or
`causal_chain_id`.

### LLM Events

- `llm_round_started`
- `llm_round_completed`
- `llm_round_failed`

Each LLM round event must include:

- `run_id`
- `agent_id`
- `turn_id`
- `round_index`
- `llm_model_used`
- `token_usage`
- `finish_reason`
- `tool_calls_returned`
- `duration_ms`

For compatibility, existing JSONL `llm_round` can map into
`llm_round_completed`.

### Tool Events

Use explicit start/completion events rather than one coarse aggregate row:

- `tool_call_started`
- `tool_call_completed`
- `tool_call_failed`
- `tool_call_blocked`

Each tool event must include:

- `run_id`
- `agent_id`
- `turn_id`
- `round_index`
- `tool_call_id`
- `meta_tool_name`
- `tool_args_preview`
- `tool_result_preview`
- `tool_args_json_redacted`
- `tool_result_json_redacted`
- `duration_ms`
- `ok`
- `error`

The existing `tool_call` event type can remain for backward compatibility, but
new Web trace UI should consume the explicit start/completed/failed types.

### Dynamic Agent Events

Dynamic multi-agent must be first-class:

- `agent_spawn_requested`
- `agent_spawned`
- `agent_completed`
- `agent_failed`
- `agent_cancelled`
- `agent_result_collected`

`agent_terminated` may remain as a legacy alias, but new events should prefer
terminal-specific names because UI can render them without inspecting
`metadata.status`.

Each dynamic agent event must include:

- `session_id`
- `turn_id`
- `causal_chain_id`
- `run_id` for the child run
- `parent_run_id`
- `agent_id`
- `parent_agent_id`
- `agent_type`
- `description`
- `status`
- `finish_reason`
- `prompt_tokens`
- `completion_tokens`
- `tool_calls`
- `spawn_tool_call_id`
- `get_result_tool_call_id` when applicable

### Harness and Evaluation Events

Harness/evaluation can use the same trace chain:

- `harness_snapshot`
- `turn_evaluation`
- `pipeline_feedback`
- `context_trace_signal`
- `session_memory_extraction`

These events must include `turn_id`, `run_id`, and `agent_id` when they happen
inside a specific run.

## Schema Changes

The clean solution is to add queryable columns to `agent_events`, rather than
burying all trace keys inside `metadata`.

Recommended additive migration:

```sql
ALTER TABLE agent_events
  ADD COLUMN run_id VARCHAR(64) NULL,
  ADD COLUMN parent_run_id VARCHAR(64) NULL,
  ADD COLUMN turn_id VARCHAR(64) NULL,
  ADD COLUMN turn_seq BIGINT NULL,
  ADD COLUMN round_index BIGINT NULL,
  ADD COLUMN tool_call_id VARCHAR(128) NULL,
  ADD COLUMN parent_agent_id VARCHAR(128) NULL,
  ADD COLUMN trace_kind VARCHAR(64) NULL;

CREATE INDEX idx_agent_events_trace
  ON agent_events (session_id, turn_id, created_at);

CREATE INDEX idx_agent_events_run
  ON agent_events (session_id, run_id, created_at);

CREATE INDEX idx_agent_events_parent_run
  ON agent_events (session_id, parent_run_id, created_at);

CREATE INDEX idx_agent_events_tool_call
  ON agent_events (session_id, tool_call_id);
```

Rationale:

- `metadata` is flexible but weak for common trace queries.
- `run_id`, `turn_id`, and `tool_call_id` will be filtered constantly by UI and
  harness.
- Additive columns do not break existing readers.

Keep large or unstable payloads in `metadata`:

- redacted tool args/result JSON
- provider-specific response data
- model settings
- planning/evaluation internals
- trace rendering hints

## Phase 1 Implementation Contract

This section is the implementation contract. If it conflicts with earlier
high-level wording, this section wins for Phase 1.

### Fixed Decisions

- Web/server trace persistence is DB-first.
- Phase 1 critical trace events are inserted directly into `agent_events`.
- Phase 1 must not rely on the current in-memory event ingestion queue for
  critical Web trace events.
- JSONL is written after the DB event as a best-effort diagnostic mirror.
- Web trace APIs read only DB data.
- `session_transcript_items` remains transcript-only.
- Dynamic subagent lifecycle is owned by `DynamicAgentSpawner`.
- Tool call detail is owned by the tool execution path, not reconstructed from
  final text.
- A partial DB trace must return `complete=false`.

### Fixed Schema Additions

Add these nullable columns to `agent_events`:

```sql
run_id VARCHAR(64) NULL
parent_run_id VARCHAR(64) NULL
turn_id VARCHAR(64) NULL
turn_seq BIGINT NULL
round_index BIGINT NULL
tool_call_id VARCHAR(128) NULL
parent_agent_id VARCHAR(128) NULL
trace_kind VARCHAR(64) NULL
```

Add these indexes:

```sql
CREATE INDEX idx_agent_events_trace
  ON agent_events (session_id, turn_id, created_at);

CREATE INDEX idx_agent_events_run
  ON agent_events (session_id, run_id, created_at);

CREATE INDEX idx_agent_events_parent_run
  ON agent_events (session_id, parent_run_id, created_at);

CREATE INDEX idx_agent_events_tool_call
  ON agent_events (session_id, tool_call_id);
```

Migration requirements:

- The migration must be idempotent.
- Existing rows may leave new columns `NULL`.
- New Web/server trace events must populate the new columns whenever the value
  exists.
- Do not make `metadata` the only storage location for these fields.

### Fixed Critical Event Set

Phase 1 must durably insert these event types for Web/server runs:

- `user_query`
- `llm_response`
- `llm_round_completed`
- `tool_call_started`
- `tool_call_completed`
- `tool_call_failed`
- `agent_spawned`
- `agent_completed`
- `agent_failed`
- `agent_cancelled`
- `agent_result_collected`

Phase 1 may also keep existing event types for compatibility, including:

- `tool_call`
- `agent_terminated`
- `pipeline_feedback`
- `turn_evaluation`

Compatibility events do not replace the fixed critical event set.

### Fixed Write Semantics

For Web/server critical trace events:

```text
1. Construct TraceEvent.
2. Insert TraceEvent into agent_events directly.
3. If insert succeeds, optionally mirror to local JSONL.
4. If insert fails, mark trace persistence degraded and surface that state.
5. Never report the trace as complete if a critical insert failed.
```

The DB insert path must use deterministic `event_id` where an event could be
retried. Recommended IDs:

```text
trace:<session_id>:<turn_id>:user_query
trace:<run_id>:round:<round_index>:completed
trace:<run_id>:tool:<tool_call_id>:started
trace:<run_id>:tool:<tool_call_id>:completed
trace:<child_run_id>:agent_spawned
trace:<child_run_id>:agent_terminal
trace:<parent_run_id>:get_result:<tool_call_id>:<child_agent_id>
```

If an event is naturally unique and never retried, a UUIDv7 is acceptable, but
tests should prefer deterministic IDs for trace assertions.

### Fixed TraceEvent Shape

Add a runtime/services shared type equivalent to:

```rust
pub struct TraceEvent {
    pub event_id: String,
    pub session_id: String,
    pub user_id: String,
    pub event_type: String,
    pub trace_kind: String,
    pub turn_id: Option<String>,
    pub turn_seq: Option<i64>,
    pub run_id: Option<String>,
    pub parent_run_id: Option<String>,
    pub agent_id: Option<String>,
    pub parent_agent_id: Option<String>,
    pub round_index: Option<i64>,
    pub tool_call_id: Option<String>,
    pub meta_tool_name: Option<String>,
    pub content: Option<String>,
    pub reasoning_content: Option<String>,
    pub token_usage: Option<serde_json::Value>,
    pub llm_model_used: Option<String>,
    pub meta_duration_ms: Option<i32>,
    pub parent_event_id: Option<String>,
    pub causal_chain_id: Option<String>,
    pub metadata: serde_json::Value,
    pub created_at: chrono::DateTime<chrono::Utc>,
}
```

`TraceEvent` is the canonical in-process event. It may map to:

- `agent_events` row.
- JSONL `JournalEvent`.
- SSE progress event, if useful.

No caller should independently construct SQL payloads for trace events.

### Fixed Payload Examples

#### `llm_round_completed`

```json
{
  "event_type": "llm_round_completed",
  "trace_kind": "llm_round",
  "session_id": "fc999d7d-adf4-4670-b43b-946d28f0c026",
  "turn_id": "turn-85d94018",
  "turn_seq": 2,
  "run_id": "85d94018-93f2-4574-b2a4-2102f4392404",
  "parent_run_id": null,
  "agent_id": "root-agent",
  "round_index": 0,
  "llm_model_used": "default",
  "token_usage": {
    "prompt": 15621,
    "completion": 381,
    "total": 16002
  },
  "metadata": {
    "finish_reason": "tool_calls",
    "tool_calls_returned": 4,
    "cache_read_tokens": 14592
  }
}
```

#### `tool_call_completed`

```json
{
  "event_type": "tool_call_completed",
  "trace_kind": "tool_call",
  "session_id": "fc999d7d-adf4-4670-b43b-946d28f0c026",
  "turn_id": "turn-85d94018",
  "run_id": "85d94018-93f2-4574-b2a4-2102f4392404",
  "agent_id": "root-agent",
  "round_index": 0,
  "tool_call_id": "tool-c0c5c9b67eba4e7faf11c9ecc96cd649",
  "meta_tool_name": "agent",
  "meta_duration_ms": 0,
  "metadata": {
    "ok": true,
    "action": "spawn",
    "args_preview": "agent.spawn: 海盗船长打招呼",
    "result_preview": "launched general-purpose_f57117c6@7fc109f5",
    "child_agent_id": "general-purpose_f57117c6@7fc109f5",
    "child_run_id": "7fc109f5-50bb-4756-b588-31a5b289d948"
  }
}
```

#### `agent_spawned`

```json
{
  "event_type": "agent_spawned",
  "trace_kind": "agent_lifecycle",
  "session_id": "fc999d7d-adf4-4670-b43b-946d28f0c026",
  "turn_id": "turn-85d94018",
  "run_id": "7fc109f5-50bb-4756-b588-31a5b289d948",
  "parent_run_id": "85d94018-93f2-4574-b2a4-2102f4392404",
  "agent_id": "general-purpose_f57117c6@7fc109f5",
  "parent_agent_id": "root-agent",
  "tool_call_id": "tool-c0c5c9b67eba4e7faf11c9ecc96cd649",
  "metadata": {
    "agent_type": "general-purpose",
    "description": "海盗船长打招呼",
    "run_in_background": true
  }
}
```

#### `agent_completed`

```json
{
  "event_type": "agent_completed",
  "trace_kind": "agent_lifecycle",
  "session_id": "fc999d7d-adf4-4670-b43b-946d28f0c026",
  "turn_id": "turn-85d94018",
  "run_id": "7fc109f5-50bb-4756-b588-31a5b289d948",
  "parent_run_id": "85d94018-93f2-4574-b2a4-2102f4392404",
  "agent_id": "general-purpose_f57117c6@7fc109f5",
  "parent_agent_id": "root-agent",
  "metadata": {
    "status": "completed",
    "finish_reason": "normal",
    "prompt_tokens": 341,
    "completion_tokens": 75,
    "tool_calls": 0,
    "result_preview": "啊嘿！扬帆起航的勇士们！..."
  }
}
```

#### `agent_result_collected`

```json
{
  "event_type": "agent_result_collected",
  "trace_kind": "agent_lifecycle",
  "session_id": "fc999d7d-adf4-4670-b43b-946d28f0c026",
  "turn_id": "turn-85d94018",
  "run_id": "85d94018-93f2-4574-b2a4-2102f4392404",
  "agent_id": "root-agent",
  "tool_call_id": "tool-ac784aeff86b4033baa32c49a2c9f76f",
  "metadata": {
    "child_agent_id": "general-purpose_f57117c6@7fc109f5",
    "child_run_id": "7fc109f5-50bb-4756-b588-31a5b289d948",
    "child_status": "completed"
  }
}
```

### Fixed Trace API Contract

Phase 1 must implement:

```text
GET /sessions/{session_id}/turns/{turn_id}/trace
```

Optional later endpoints:

```text
GET /sessions/{session_id}/trace
GET /runs/{run_id}/trace
```

The Phase 1 response shape is:

```json
{
  "session_id": "fc999d7d-adf4-4670-b43b-946d28f0c026",
  "turn_id": "turn-85d94018",
  "turn_seq": 2,
  "source": "database",
  "complete": true,
  "warnings": [],
  "missing": [],
  "events": [],
  "tree": {
    "type": "turn",
    "run_id": "85d94018-93f2-4574-b2a4-2102f4392404",
    "agent_id": "root-agent",
    "rounds": [
      {
        "round_index": 0,
        "events": [],
        "tool_calls": []
      }
    ],
    "children": [
      {
        "run_id": "7fc109f5-50bb-4756-b588-31a5b289d948",
        "parent_run_id": "85d94018-93f2-4574-b2a4-2102f4392404",
        "agent_id": "general-purpose_f57117c6@7fc109f5",
        "status": "completed",
        "events": []
      }
    ]
  }
}
```

Completeness rules for Phase 1:

- Missing root `user_query` means `complete=false`.
- Missing final parent `llm_response` means `complete=false` unless the turn is
  still running.
- A `tool_call_completed` with `action=spawn` and `child_run_id` requires a
  matching `agent_spawned`.
- An `agent_spawned` requires one terminal lifecycle event:
  `agent_completed`, `agent_failed`, or `agent_cancelled`.
- An `agent_completed` that was collected by parent must have
  `agent_result_collected`.
- Any recorded `trace_persistence_degraded` event means `complete=false`.

### Fixed Phase 1 Test Checklist

Implementation is not complete until these tests exist and pass:

- Schema test: new `agent_events` columns and indexes are created
  idempotently.
- Mapping test: every fixed critical event maps to `agent_events` columns, not
  only metadata.
- Redaction test: tool args/result DB payloads are redacted/truncated.
- Web no-tool E2E: one input persists root `user_query`,
  `llm_round_completed`, final `llm_response`, and transcript rows.
- Web tool E2E: one tool call persists `tool_call_started` and
  `tool_call_completed` with `tool_call_id`.
- Web multi-agent E2E: four background `agent.spawn` calls persist four
  `agent_spawned`, four terminal lifecycle events, four
  `agent_result_collected`, and child run lineage.
- Trace API E2E: querying by `session_id + turn_id` returns `complete=true`
  for the multi-agent run.
- Completeness negative test: delete or omit one `agent_completed`; trace API
  returns `complete=false` and names the missing event.
- No-JSONL dependency test: remove or redirect local journal after the run;
  trace API still reconstructs from DB.

## Write Path

### Required Abstraction

Introduce one shared trace writer abstraction:

```rust
#[async_trait]
pub trait TraceEventWriter: Send + Sync {
    async fn write(&self, event: TraceEvent) -> Result<(), TraceWriteError>;
    async fn write_many(&self, events: Vec<TraceEvent>) -> Result<(), TraceWriteError>;
}
```

Implementations:

- `LocalJournalTraceWriter`: appends to session JSONL.
- `DatabaseTraceWriter`: inserts or enqueues into `agent_events`.
- `CompositeTraceWriter`: writes to both, with local journal first.
- `DurableWebTraceWriter`: DB-required writer for Web/server sessions.

The runtime should call `TraceEventWriter`, not `JournalWriter` and
`agent_events` separately.

### Web Durability Mode

Web/server sessions must use DB-required trace persistence. The local JSONL
writer may be enabled as an additional mirror, but the product trace must not
depend on it.

Recommended mode split:

```text
CLI / local dev
  Local journal is always written.
  DB ingestion is optional when cloud sync is configured.

Web / server
  DB trace write is required.
  Local journal is optional diagnostic output.
  Trace API reads DB only.
```

The Web writer should not rely only on an in-memory ingestion channel. A
short-lived worker can die after enqueueing but before flush. Critical Web
trace events need one of these durable write guarantees:

- direct `agent_events` insert in the request/run process for critical events;
- durable outbox table in the same DB transaction, drained asynchronously;
- existing event ingestion only if the enqueue itself is backed by a durable DB
  outbox rather than process memory.

Critical events:

- `user_query`
- final parent `llm_response`
- `llm_round_completed`
- `tool_call_completed` / `tool_call_failed`
- `agent_spawned`
- terminal dynamic-agent lifecycle event
- `agent_result_collected`

### Failure Semantics

For Web, DB is durability and queryability. Local JSONL is diagnostic only.
For CLI, local JSONL can remain local durability with optional DB sync.

Write behavior:

1. Build one canonical `TraceEvent`.
2. For Web/server, persist the critical DB trace event durably.
3. Optionally append the same event to local JSONL as a mirror.
4. If DB persistence fails, mark the run/session trace as degraded and surface
   it through API/health/UI.
5. Do not fabricate DB success.
6. Do not return `complete=true` from trace API unless DB has the required
   event chain.

This is not a silent fallback: local-only trace state must be visible as
`trace_persistence_degraded`, and Web UI must not present it as a complete
session trace. In strict harness or production modes, failing to persist
critical trace events may be configured to fail the run instead of continuing.

### Redaction

Trace events must use existing redaction policy before DB persistence.

Rules:

- Full local JSONL may keep richer debug detail subject to local config.
- DB should store previews and redacted JSON payloads by default.
- Secrets must never be queryable in `agent_events`.
- Tool result bodies above a size threshold should be truncated and stored as
  `preview + artifact_ref` if artifact retention is configured.

## Runtime Integration Points

### Server Loop

`ServerAgenticLoopHost` should emit:

- `llm_round_started`
- `llm_round_completed`
- `llm_round_failed`
- tool call events for every tool call with args/result linkage

The current post-loop `persist_server_loop_tool_events` should be replaced or
reduced to a compatibility aggregate. It must not remain the only source of
tool trace detail.

### Server Tool Executor

`ServerToolExecutor` should emit:

- `tool_call_started`
- `tool_call_completed`
- `tool_call_failed`
- `tool_call_blocked`

For `agent` tool calls, include normalized action:

- `spawn`
- `get_result`
- `send_message`
- `run_chain`

For `agent.spawn`, include returned `child_agent_id` and `child_run_id` when
available.

### Dynamic Agent Spawner

`DynamicAgentSpawner` should emit lifecycle trace events through the shared
writer:

- `agent_spawn_requested` before side effects.
- `agent_spawned` after state registration succeeds.
- `agent_completed` when child returns normal completion.
- `agent_failed` when child errors.
- `agent_cancelled` when cancelled.
- `agent_result_collected` when parent calls `get_result`.

`DynamicAgentSpawner` is the correct owner of these events because it owns
agent lifecycle state. The server executor should not duplicate lifecycle
events.

### Server Spawn Executor

`ServerSpawnAgentExecutor` should pass trace context into child
`AgenticLoopState`:

- parent `turn_id`
- parent `run_id`
- child `run_id`
- child `agent_id`
- `causal_chain_id`
- `spawn_tool_call_id`

Child core events (`user_query`, `llm_response`, `llm_round_*`) must use the
child run id but preserve parent lineage.

### Transcript Persistence

`persist_session_transcript_items` should continue to write user/assistant
visible content.

It should not be used as evidence of lifecycle events. It may store child
prompt/response rows when child runs produce user-visible transcript entries,
but trace UI must not require this.

## Read Path and API

Web UI should read trace data through runtime APIs.

### API Endpoints

Add:

```text
GET /sessions/{session_id}/trace
GET /sessions/{session_id}/turns/{turn_id}/trace
GET /runs/{run_id}/trace
```

Optional filters:

- `include_payloads=true|false`
- `include_tool_results=true|false`
- `event_types=...`
- `limit=...`
- `after_event_id=...`

### Response Shape

The server should return both raw events and a normalized tree.

```json
{
  "session_id": "fc999d7d-adf4-4670-b43b-946d28f0c026",
  "turn_id": "turn-...",
  "root_event_id": "event-...",
  "events": [],
  "tree": {
    "type": "turn",
    "input": {
      "event_id": "event-...",
      "content_preview": "启动多个 agent ..."
    },
    "parent_run": {
      "run_id": "85d94018-93f2-4574-b2a4-2102f4392404",
      "agent_id": "root-agent",
      "rounds": [],
      "children": []
    }
  },
  "completeness": {
    "source": "database",
    "complete": true,
    "missing_event_types": [],
    "warnings": []
  }
}
```

### Completeness Contract

The trace API must report completeness explicitly.

Examples:

- `complete=true`: DB has root, rounds, tool calls, child lifecycle, and final
  response.
- `complete=false`: DB is missing expected lifecycle or tool events.
- `source="database"`: the only normal Web UI path.
- `source="local_jsonl"`: diagnostic/admin path only, not product session state.
- `source="database_with_degraded_persistence"`: DB has a partial trace and the
  runtime recorded DB write failures.

Do not silently produce a partial tree that looks complete.

## UI Model

Web UI can render a trace drawer or timeline for each user input.

Suggested sections:

- Input and final answer.
- Parent run timeline.
- LLM rounds with model, tokens, duration, finish reason.
- Tool calls with args/result previews.
- Subagent tree with status, duration, token usage, and result preview.
- Warnings for missing/partial trace data.

The UI should link from visible chat messages to `turn_id`, not infer the trace
by scanning text.

## Backfill and Migration

Existing sessions cannot be perfectly backfilled from DB alone because DB is
missing lifecycle details. For local development, a one-time diagnostic
backfill may parse JSONL and insert missing `agent_spawned`/`agent_completed`
events into `agent_events`, but this must be an explicit tool, not normal Web
UI behavior.

Backfill rules:

- Mark inserted rows with `metadata.backfilled_from_jsonl=true`.
- Preserve original journal timestamp where available.
- Use deterministic event IDs derived from `(session_id, journal_event_id)`.
- Do not overwrite existing DB events.
- Do not backfill secrets into DB.
- Do not make Web UI depend on backfill to render new sessions.

## Testing Plan

### Unit Tests

- `TraceEvent` maps to `JournalEvent` without losing trace IDs.
- `TraceEvent` maps to `agent_events` columns and metadata.
- Redaction removes secrets from DB payloads.
- `DynamicAgentSpawner` emits lifecycle events exactly once.
- Tool call completed events include `tool_call_id`, `round_index`, and
  `meta_tool_name`.
- Trace tree builder handles out-of-order DB events deterministically.
- Trace tree builder reports missing lifecycle events as incomplete.

### Integration Tests

- Web server turn with no tools persists:
  `user_query`, `llm_round_completed`, final `llm_response`, transcript rows.
- Web server turn with one tool persists:
  `tool_call_started`, `tool_call_completed`, args/result previews.
- Web dynamic multi-agent turn persists:
  N `agent_spawned`, N child `llm_response`, N `agent_completed`, N
  `agent_result_collected`.
- DB trace API reconstructs parent/child tree from `agent_events` only.
- Local JSONL and DB trace contain matching event IDs for shared events.

### E2E Acceptance Test

Scenario:

```text
User: Use four agents with different identities to greet me, then summarize.
```

Expected DB assertions:

- One root `user_query`.
- One parent run id.
- Four `agent_spawned` events.
- Four child run ids with `parent_run_id = parent_run_id`.
- Four child `user_query` events.
- Four child `llm_response` events.
- Four terminal agent lifecycle events.
- Four `agent_result_collected` events.
- One final parent `llm_response`.
- Trace API returns `complete=true`.

## Rollout Plan

### Slice 1: Trace Contract

- Add `TraceEvent` and `TraceEventWriter`.
- Implement JSONL and DB mappings.
- Add tests for event serialization and DB mapping.

### Slice 2: Schema Migration

- Add trace columns to `agent_events`.
- Add indexes for `session_id + turn_id`, `session_id + run_id`, and
  `session_id + parent_run_id`.
- Keep existing readers compatible.

### Slice 3: Server Loop Events

- Emit LLM round events through `TraceEventWriter`.
- Emit detailed tool call events.
- Keep existing aggregate tool rows only if needed for compatibility.

### Slice 4: Dynamic Agent Lifecycle

- Wire `DynamicAgentSpawner` lifecycle events into `TraceEventWriter`.
- Persist `agent_spawned`, terminal lifecycle, and `agent_result_collected`.
- Add Web multi-agent DB assertions.

### Slice 5: Trace Query API

- Add trace service queries over `agent_events`.
- Build normalized turn/run tree.
- Add completeness reporting.

### Slice 6: Web UI

- Add per-message trace affordance.
- Render trace timeline/tree from API.
- Show completeness warnings clearly.

## Acceptance Criteria

- Given a Web user input, runtime API can reconstruct the complete trace from
  DB without reading local JSONL.
- Web session trace durability survives container restart, sandbox teardown, and
  local filesystem loss.
- Critical trace events are not only buffered in process memory before being
  considered persisted.
- Dynamic subagent lifecycle is visible in `agent_events`.
- Tool calls include enough structured detail to explain args/result linkage.
- Transcript tables remain transcript-only and are not used as trace source of
  truth.
- Local JSONL remains available and shares event IDs with DB events where the
  event is common.
- Tests fail if `agent_spawned`, terminal lifecycle events, or tool call detail
  stop being persisted.

## Prohibited Patterns

- Do not make Web UI parse `~/.astra/sessions/*.jsonl`.
- Do not use local JSONL as Web session state or Web trace source of truth.
- Do not rely on process-local ingestion queues as the only persistence for
  critical Web trace events.
- Do not infer subagent execution from final answer text.
- Do not rely on `session_transcript_items` to reconstruct agent lifecycle.
- Do not write lifecycle events separately in every executor.
- Do not create a second event taxonomy just for Web UI.
- Do not store full unredacted tool args/results in DB by default.
- Do not silently return partial traces as complete.
