# Edge-Cloud Split Execution

> **Status**: Core Design — single source of truth for edge-cloud execution model
> **Last Updated**: 2026-03-01
> **Related**: [deployment-architecture.md §1.1](deployment-architecture.md), [skills-and-tools.md](skills-and-tools.md), [agents-and-orchestration.md](agents-and-orchestration.md), [durable-agent-runs.md](durable-agent-runs.md)

---

## 1. The Problem

Agent tools need the user's filesystem. The server doesn't have it.

```
read_file("/home/alice/project/main.go")   → must run on Alice's machine
bash("go test ./...")                       → must run on Alice's machine
git_diff()                                  → must run on Alice's machine
memory_search("how did we fix auth?")       → must run on server (data in MatrixOne)
LLM call                                   → must run on server (API key security + enrichment)
```

No single execution location works for everything. The agentic loop must be **split**.

---

## 2. Execution Split

### What runs where

| Execution Location | What | Why |
|---|---|---|
| **Edge** (user's machine) | File ops, shell, git, grep, glob, MCP servers | Needs local filesystem |
| **Edge** | Agentic loop driver (EdgeChatLoop) | Must call local tools between LLM turns |
| **Edge** | Permission prompts (Y/N/Always/Deny) | Interactive, needs user's terminal |
| **Edge** | Terminal rendering | User's terminal |
| **Edge** | Project rules loading (.mo-agent/rules.md, CLAUDE.md) | Local files |
| **Cloud** | LLM call | API key security; context enrichment; prompt caching |
| **Cloud** | Context assembly (memory search, few-shot, skill index) | Data in MatrixOne |
| **Cloud** | Model routing, SLO escalation | Historical cost/quality data in DB |
| **Cloud** | Budget control, rate limiting | Per-user enforcement |
| **Cloud** | Audit logging (decision + context snapshot) | Source of truth in MatrixOne |
| **Cloud** | Firewall verification | Needs context snapshot for claim verification |
| **Cloud** | Event persistence | All events → MatrixOne |
| **Cloud** | Skill catalog (definitions, versions) | Source of truth; edge caches |

### What the cloud does per `/chat/turn` (not just proxying)

```
Edge sends: {session_id, messages, tool_results, project_rules?}
                                    │
Cloud receives                      ▼
  1. Auth (JWT verify)
  2. Rate limit check
  3. Persist tool_result events from edge
  4. Context assembly:
     ├── Memory search (episodic, semantic, procedural)
     ├── Few-shot retrieval (similar past interactions)
     ├── Cross-session context (continuity)
     └── Skill index (available tools for LLM)
  5. Prompt enrichment:
     ├── Inject memory into system prompt
     ├── Inject project rules (from edge, first turn)
     ├── Inject few-shot examples
     └── Token budget allocation
  6. Model routing:
     ├── Select model by task complexity
     ├── SLO escalation if needed
     └── Cost estimation
  7. Budget gate: reject if user's budget exhausted
  8. LLM call (streaming, API key server-side)
  9. Post-LLM:
     ├── Firewall verification (claims vs context snapshot)
     ├── Confidence scoring
     ├── Cost tracking
     └── Persistence:
         ├── Context snapshot + DecisionAudit (links decision to snapshot)
         ├── SkillSelectionEvent (when tool_calls present)
         ├── Observations via Observer (background thread, LLM extraction)
         └── Implicit feedback detection (heuristic, zero LLM cost)
  10. Return SSE stream: {text_deltas, tool_calls, usage}
```

---

## 3. The `/chat/turn` Protocol

### Request

```
POST /chat/turn
Authorization: Bearer <jwt>
Content-Type: application/json

{
  "session_id": "ses_abc123",          // required (except first turn, auto-created)
  "messages": [                         // current conversation (edge's view)
    {"role": "user", "content": "fix the bug in auth.go"}
  ],
  "tool_results": [                     // results from edge-executed tools (empty on first turn)
    {
      "tool_call_id": "tc_001",
      "name": "read_file",
      "result": "package auth\n\nfunc Login()..."
    }
  ],
  "project_rules": "# Rules\n...",     // optional, first turn only (cloud caches per session)
  "agent_id": "dev-agent",             // optional
  "model": "claude-sonnet-4-20250514"  // optional override
}
```

### Response (SSE stream)

```
data: {"type": "ping", "ts": 1709251200123}

data: {"type": "session_info", "session_id": "ses_abc123", "run_id": "run_xyz"}

data: {"type": "ping", "ts": 1709251215456}

data: {"type": "text_delta", "content": "Let me read"}
data: {"type": "text_delta", "content": " the auth file..."}

data: {"type": "tool_call", "id": "tc_002", "name": "read_file", "arguments": {"path": "auth.go"}}
data: {"type": "tool_call", "id": "tc_003", "name": "grep", "arguments": {"pattern": "Login", "path": "."}}

data: {"type": "usage", "prompt_tokens": 1523, "completion_tokens": 89, "cache_read_tokens": 400}
data: {"type": "turn_complete", "has_tool_calls": true}
```

### Event types

| Type | Payload | Meaning |
|---|---|---|
| `ping` | `ts` (unix epoch ms) | Server keepalive during processing (§3.1) |
| `session_info` | `session_id`, `run_id` | Session/run identifiers (first application event; pings may precede it) |
| `text_delta` | `content` | Incremental LLM text output |
| `tool_call` | `id`, `name`, `arguments` | LLM requests edge tool execution |
| `usage` | `prompt_tokens`, `completion_tokens`, `cache_read_tokens` | Token usage for this turn |
| `warning` | `message`, `claims_failed` | Firewall verification warning |
| `error` | `message`, `code`, `retryable` | Error (may be retryable) |
| `turn_complete` | `has_tool_calls` | Turn finished (always last application event) |

**Event ordering contract**: Clients MUST NOT assume ordering of event types
beyond `turn_complete` being last. `ping` events can appear anywhere —
before `session_info`, between `text_delta` chunks, or between `tool_call`
events. Clients should dispatch solely on `event["type"]`, not on position.

**Turn semantics**:
- `has_tool_calls: true` → edge must execute tools and call `/chat/turn` again with results
- `has_tool_calls: false` → final answer, conversation turn complete

### 3.1 SSE Heartbeat: Keeping Long-Running Turns Alive

The `/chat/turn` SSE stream has dead zones where no events are emitted.
The httpx client has `read=30.0` (30 seconds between data chunks). When a
dead zone exceeds 30 seconds, the client raises `ReadTimeout`.

#### 3.1.1 Dead Zones

```
Actual timeline of a /chat/turn request:

  0ms     HTTP 200 + headers
  ─── dead zone 1: context assembly ───
  ~5ms    _ensure_session (DB)
  ~50ms   _build_turn_messages (DB, memory search, prompt assembly)
  ~100ms  _get_shared_embed_fn (embedding init, cold start only)
  ─── dead zone 1 ends ───
  ~150ms  yield session_info          ← first SSE event, client read timer starts
  ─── dead zone 2: LLM TTFB ───
  ~200ms  LLM provider receives request
  ...     LLM thinking                (provider-dependent, 1-60+ seconds)
  ─── dead zone 2 ends ───
  ~35s    first text_delta
```

1. **Context assembly** (HTTP 200 → `session_info`): Typically ~50-200ms in
   warm steady state. However, `_build_turn_messages` includes memory search
   (embedding the query + vector similarity + fulltext search against
   MatrixOne), and `_get_shared_embed_fn()` loads the embedding model on
   first call. On cold start or under load, this phase can take 2-5 seconds.
   The `session_id` is known after `_ensure_session` (line ~1054), well before
   `_build_sync` runs, but `session_info` is yielded after `_build_sync`.

2. **LLM TTFB** (`session_info` → first `text_delta`): Provider-dependent,
   1-60+ seconds. This is the primary cause of `ReadTimeout`.

**Why increasing read timeout is wrong:**
- A 120s timeout can't distinguish "LLM is thinking" from "server crashed"
- The user sees no feedback for up to 2 minutes
- The real timeout value is unknowable — some turns take 5s, others 90s

#### 3.1.2 Design: Server-Side Heartbeat

The server emits periodic `ping` events (see event type table in §3) during
dead zones. The client ignores them for rendering but they keep the TCP
connection alive and the read timer resetting.

**Event format:**

```
data: {"type":"ping","ts":1709251200123}
```

- `type: "ping"` — distinct from all application event types
- `ts` — Unix epoch milliseconds (for log correlation)
- Minimal payload (~45 bytes serialized) — no meaningful bandwidth cost

**Why a JSON event, not an SSE comment?**

SSE comments (`: heartbeat\n\n`) are invisible to `httpx-sse`'s `aiter_sse()` —
it only yields `data:` lines. We'd need to switch to raw line parsing or a
different SSE library. A typed JSON event works with the existing client
infrastructure and is explicitly filterable.

#### 3.1.3 Architecture: Wrap the Entire Generator

The heartbeat must cover **both** dead zones (context assembly and LLM TTFB).
Rather than inserting heartbeat logic into each phase, we wrap the entire
`event_generator()` output with a heartbeat layer:

```
event_generator()          heartbeat_wrapper()           Client
  │                          │                             │
  │ (context assembly...)    │ no event for 15s →          │
  │                          │ yield ping                  │ ← timer reset
  │ yield session_info  ───→ │ yield session_info     ───→ │ ← timer reset
  │ (LLM thinking...)       │ no event for 15s →          │
  │                          │ yield ping                  │ ← timer reset
  │ yield text_delta    ───→ │ yield text_delta       ───→ │ ← timer reset
  │ yield text_delta    ───→ │ yield text_delta       ───→ │
  │ yield turn_complete ───→ │ yield turn_complete    ───→ │
```

This is a clean separation: `event_generator()` has zero heartbeat awareness,
and the wrapper is reusable across all SSE endpoints.

#### 3.1.4 The Blocking Problem

The LLM streaming layer uses synchronous iterators internally
(`provider.complete_with_tools_stream()` returns `Iterator[dict]`), wrapped
in an async generator that does `for chunk in sync_iter: yield chunk`. This
means each `__anext__()` call blocks the event loop until the synchronous
provider yields the next chunk.

This creates a fundamental problem for any heartbeat approach that shares
the event loop with the generator:

- **Timeout-based polling** (`asyncio.wait_for(__anext__(), timeout=N)`):
  Cannot interrupt a synchronous block — the timeout never fires.
- **Queue + `create_task`**: The drain task shares the event loop. When it
  calls `__anext__()` on the generator, it blocks the event loop, preventing
  the outer loop's `wait_for(queue.get())` from timing out.
- **Thread-based queue**: The inner generator is async (uses `await`,
  `yield`, `async for`) — it can't run in a plain thread without its own
  event loop, adding unacceptable complexity.

All fail for the same reason: the synchronous LLM iteration monopolizes
the event loop thread, and moving the async generator to another thread
is impractical.

#### 3.1.5 Solution: Fix the Root Cause + Queue Wrapper

The cleanest solution is two parts:

**Part 1: Stop blocking the event loop.** Wrap the synchronous provider
iteration in `asyncio.to_thread` at the source — inside
`chat_with_tools_stream`:

```python
# In core/llm/client.py — chat_with_tools_stream
# Before (blocks event loop):
for chunk in provider.complete_with_tools_stream(...):
    yield chunk

# After (yields control between chunks):
_END = object()  # module-level sentinel (separate from _SENTINEL in chat.py)
sync_iter = provider.complete_with_tools_stream(...)
while True:
    chunk = await asyncio.to_thread(next, sync_iter, _END)
    if chunk is _END:
        break
    yield chunk
```

Each `next()` call on the synchronous iterator runs in a thread pool,
freeing the event loop. The async generator yields control between chunks,
allowing other coroutines (including the heartbeat wrapper) to run. This
change is independently beneficial — it unblocks the event loop for all
concurrent work, not just heartbeats.

**Part 2: Queue-based heartbeat wrapper.** With the event loop unblocked,
a simple `create_task` + Queue approach works:

```python
HEARTBEAT_INTERVAL_S = 15

_SENTINEL = object()


def _sse_ping() -> str:
    return f"data: {json.dumps({'type':'ping','ts':int(time.time()*1000)})}\n\n"


async def _with_heartbeat(
    sse_generator: AsyncIterator[str],
) -> AsyncIterator[str]:
    """Wrap an SSE generator with periodic ping events.

    Runs the inner generator in a background task that pushes events
    into a queue. The outer loop drains the queue with a timeout —
    if no event arrives within HEARTBEAT_INTERVAL_S, emits a ping.

    Prerequisite: the inner generator must not block the event loop.
    Synchronous I/O inside the generator must be wrapped in
    asyncio.to_thread (see core/llm/client.py).

    Exceptions from the inner generator propagate to the caller:
    _drain puts the exception object into the queue, and the outer
    loop re-raises it.
    """
    queue: asyncio.Queue[str | BaseException | object] = asyncio.Queue()

    async def _drain():
        try:
            async for line in sse_generator:
                await queue.put(line)
        except asyncio.CancelledError:
            raise  # let cancellation propagate naturally
        except BaseException as exc:
            await queue.put(exc)
        finally:
            await queue.put(_SENTINEL)

    task = asyncio.create_task(_drain())
    try:
        while True:
            try:
                item = await asyncio.wait_for(
                    queue.get(), timeout=HEARTBEAT_INTERVAL_S,
                )
            except asyncio.TimeoutError:
                yield _sse_ping()
                continue
            if item is _SENTINEL:
                break
            if isinstance(item, BaseException):
                raise item
            yield item
    finally:
        task.cancel()
        try:
            await task
        except asyncio.CancelledError:
            pass
```

**Exception propagation**: If the inner generator raises (e.g., DB error
before any yield), `_drain` catches the exception and puts it in the queue.
The outer loop receives it and re-raises, so the caller sees the original
exception. For generators like `event_generator()` that catch their own
exceptions and yield error SSE events, the error event is queued normally
and the generator ends cleanly — the exception path is never hit. But
`_with_heartbeat` is a generic wrapper, so it must handle both patterns.

#### 3.1.6 Heartbeat Interval

**15 seconds.**

- Read timeout is 30s → heartbeat at 15s gives 2× safety margin
- Not too frequent (every 1s would be noisy in logs/debug)
- Not too infrequent (25s would leave only 5s margin for jitter)

This is a server constant, not configurable. If the read timeout changes,
the heartbeat interval should be updated to match (roughly half).

#### 3.1.7 Scope

| Endpoint | Heartbeat? | Notes |
|---|---|---|
| `POST /chat/turn` | Yes | Primary use case |
| `POST /chat/stream` | Yes | Same pipeline |
| `GET /chat/runs/{id}/stream` | Yes | Run may be queued |
| `POST /streaming/chat` | No | Deprecated — clients should migrate to `/chat/stream` |

#### 3.1.8 Client Behavior

`_consume_turn` in `edge_chat_loop.py` already dispatches on `event["type"]`.
Add one case:

```python
elif etype == "ping":
    pass  # heartbeat — connection kept alive, nothing to render
```

The httpx read timer resets on every received SSE event, including pings.
No timeout configuration change needed.

#### 3.1.9 Observability

Ping events are transport-level keepalives, not application events:

- **Metrics**: Exclude `type=ping` from event count metrics. Tag separately
  if ping frequency is tracked (indicates slow LLM turns).
- **Access logs**: No special treatment needed — pings are part of the SSE
  response body, not separate HTTP requests.
- **Event persistence**: Pings are NOT persisted to MatrixOne. They exist
  only in the SSE wire format.

#### 3.1.10 Application-Level Timeout

The heartbeat solves the *transport* timeout but not the *application* timeout.
A turn that pings for 10 minutes is alive but broken. The edge chat loop
should enforce a maximum wall-clock time per turn:

```python
turn_start = time.monotonic()
async for event in api_client.chat_turn(...):
    if time.monotonic() - turn_start > MAX_TURN_WALL_CLOCK_S:
        renderer.error("Turn timed out")
        break
    ...
```

**`MAX_TURN_WALL_CLOCK_S`: 300 seconds (5 minutes).**

**What happens on timeout:**

When the client `break`s out of the `async for`, httpx closes the underlying
HTTP connection. The server's `StreamingResponse` detects the client
disconnect (the next `yield` raises `ConnectionError` or similar), and the
generator exits. For `/chat/turn`, this is sufficient — there is no
long-lived server-side run to cancel (unlike `/chat/stream` which creates
a `RunEngine` task).

For `/chat/stream` and `/chat/runs/{id}/stream`, the client should also call
`DELETE /chat/runs/{run_id}` to cancel the server-side run and stop wasting
LLM tokens. The `run_id` is available from the `session_info` event (if
received before timeout) or from the initial `POST /chat` response.

**Server-side timeout protection:**

The client-side timeout alone is insufficient — if the client crashes or
loses network, the server continues the LLM call indefinitely, wasting
tokens. The server must independently enforce a hard wall-clock limit:

- `event_generator()` tracks elapsed time from request start
- At `SERVER_TURN_TIMEOUT_S = 240` (4 minutes, below client's 300s to
  ensure the server times out first), yield an error event and exit:
  `{"type": "error", "message": "Turn exceeded server time limit", "code": "turn_timeout", "retryable": false}`
- For `/chat/stream`: mark the run as `status=timeout`, allowing future
  resume via `durable-agent-runs` if implemented

#### 3.1.11 Proxy and Infrastructure Considerations

Reverse proxies (nginx, ALB, CloudFront) have their own idle connection
timeouts independent of TCP-level data flow:

- **nginx**: `proxy_read_timeout` (default 60s) — must be ≥ `read_timeout`
- **AWS ALB**: idle timeout (default 60s) — configurable up to 4000s
- **CloudFront**: origin response timeout (default 30s) applies to TTFB only;
  once streaming starts, CloudFront uses a 60s idle timeout between chunks

SSE pings keep the HTTP response body flowing, which satisfies most proxy
idle timeout checks. However, some proxies buffer responses and only forward
complete chunks. Ensure:

- `X-Accel-Buffering: no` header (already set in `SSE_HEADERS`)
- Proxy response buffering disabled for SSE endpoints
- Proxy idle timeout ≥ 60s for SSE endpoints

#### 3.1.12 Failure Modes

| Scenario | Behavior |
|---|---|
| LLM responds in < 15s | No pings emitted. Zero overhead. |
| LLM responds in 45s | 2 pings emitted (at 15s and 30s). Client stays alive. |
| Context assembly takes 20s (cold start) | 1 ping emitted. Client stays alive. |
| LLM hangs forever | Pings every 15s. Client app-level timeout at 300s. |
| Server process crashes | No more pings. Client read timeout fires after 30s. Correct. |
| Network partition | Same as crash from client's perspective. |

#### 3.1.13 What We Are NOT Doing

- **Changing read timeout to a large value** — masks real failures, no user feedback
- **SSE comments (`: heartbeat`)** — invisible to httpx-sse, requires library change
- **WebSocket upgrade** — unnecessary complexity for a unidirectional stream
- **Client-side retry on ReadTimeout** — wrong layer; server should keep connection alive
- **Heartbeat for deprecated `/streaming/chat`** — clients should migrate
- **Option B (timeout-based `__anext__` polling)** — cannot interrupt synchronous blocking; see §3.1.4
- **Queue wrapper without fixing sync blocking** — `create_task(_drain())` shares the event loop; sync blocking in the inner generator prevents the outer loop from timing out; see §3.1.4

#### 3.1.14 Implementation Checklist

LLM client (`core/llm/client.py`):
- [ ] Wrap `for chunk in provider.complete_with_tools_stream(...)` with `asyncio.to_thread(next, ...)` loop in `chat_with_tools_stream`
- [ ] Same for `chat_stream` if it has synchronous iteration

Server (`api/routers/chat.py`):
- [ ] Add `_with_heartbeat()` wrapper (reusable across endpoints)
- [ ] Add `_sse_ping()` helper
- [ ] Add `HEARTBEAT_INTERVAL_S = 15` constant
- [ ] Wrap `event_generator()` in `chat_turn` with `_with_heartbeat()`
- [ ] Wrap `event_generator()` in `chat_stream` with `_with_heartbeat()`
- [ ] Wrap `stream_run_events` generator in `stream_agent_run` with `_with_heartbeat()`
- [ ] Follow-up: move `_get_shared_embed_fn()` into `_build_sync` and `_ensure_session` into `asyncio.to_thread` to eliminate remaining sync blocking in the event loop (sub-second, not a timeout risk, but consistent with the design principle)
- [ ] Add `SERVER_TURN_TIMEOUT_S = 240` in `event_generator()` — yield error event and exit on timeout

Client (`cli/edge_chat_loop.py`):
- [ ] Handle `"ping"` event type in `_consume_turn` (ignore, no effect on TurnResult)
- [ ] Add `MAX_TURN_WALL_CLOCK_S = 300` and enforce in the turn loop

Client (`cli/api_client.py`):
- [ ] No changes needed — read timeout stays at 30s

Tests:
- [ ] LLM client: `chat_with_tools_stream` no longer blocks event loop (concurrent task can run during iteration)
- [ ] Server: `_with_heartbeat` emits pings when inner async generator sleeps (simulates LLM TTFB)
- [ ] Server: `_with_heartbeat` emits pings during real slow generator (validates end-to-end with `to_thread`-based LLM iteration)
- [ ] Server: multiple pings in sequence (inner generator blocks for 50s → 3 pings at 15s, 30s, 45s)
- [ ] Server: no pings when inner generator yields faster than heartbeat interval
- [ ] Server: sentinel terminates wrapper cleanly
- [ ] Server: background task cancelled on wrapper exit (e.g., client disconnect)
- [ ] Server: exception from inner generator propagates through queue to caller
- [ ] Client: ping events ignored by `_consume_turn` (no effect on TurnResult)
- [ ] Client: app-level timeout breaks out of turn loop
- [ ] Client: app-level timeout on `/chat/stream` attempts `DELETE /chat/runs/{run_id}`
- [ ] Server: `SERVER_TURN_TIMEOUT_S` yields error event and exits generator

---

### 3.2 Follow-Up Items

Improvements identified during review, out of scope for the heartbeat design
but tracked here for future work:

1. **Protocol-level idempotency and reconnection** — Add optional
   `last_turn_id` / `resume_from_event_id` to the request, SSE `id:` fields
   on response events, and short-lived server-side caching of recent
   tool_calls results. Scope: `durable-agent-runs.md`.

2. **Thinking / progress events** — Yield `{"type": "thinking", "phase": "..."}` during
   context assembly and `{"type": "tool_executing", ...}` during edge tool
   runs. Orthogonal to heartbeat (requires changes inside `event_generator()`,
   not the wrapper). Significant UX improvement.

3. **`project_rules` hash optimization** — Return `rules_hash` in
   `session_info`, let edge skip re-sending unchanged rules on new sessions.
   Low priority (rules are a few KB, sent once per session).

4. **OpenTelemetry tracing** — Instrument `context_assembly`, `llm_call`,
   `tool_execution` as spans for latency debugging.
