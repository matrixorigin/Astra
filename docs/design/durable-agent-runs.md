# Execution Architecture: Durable Agent Loops

> **Status**: Design — addresses the fundamental gap between request-response ChatLoop and real-world complex tasks  
> **Last Updated**: 2026-02-21  
> **Supersedes**: deployment-architecture.md § 3 (Execution Model) for agent execution concerns

---

## 1. The Problem

The current ChatLoop is a **single-request synchronous loop**:

```
User message → [LLM → tool → LLM → tool → ... → LLM] → Response
                └──────── all within one HTTP request ────────┘
```

This breaks for real-world complex tasks:

| Scenario | Why it breaks |
|----------|--------------|
| "收集语料，训练模型，发布 v2" | Steps take hours; HTTP connection dies |
| "写代码，让 3 个 reviewer 从不同角度 review，修改，再 review" | Multi-agent coordination across many turns |
| "修复 CI，跑回归测试，确认通过后合并" | Needs to wait for external events (CI finish) |
| "每天检查 drift，超过阈值自动重训" | Scheduled, no user trigger |

**Root causes:**
1. Agent execution is bound to a single HTTP request lifecycle
2. Plan state lives only in memory — lost when request ends
3. Agent cannot "wait" for async events (job completion, CI, external webhook)
4. Agent cannot be triggered by anything other than a user message
5. Multi-agent coordination is synchronous (delegate_task blocks)

---

## 2. Industry Analysis

### Codex CLI / Codex App (OpenAI, Feb 2026)

**Architecture:** Agent loop runs locally (CLI) or in cloud container (Codex Web). Each "turn" is one user message → agent works until assistant message. Long-running work happens because the agent process stays alive — it's not request-response.

**Key insight:** Codex App Server uses three primitives:
- **Item**: atomic unit of I/O with lifecycle (started → delta → completed)
- **Turn**: sequence of items from one unit of agent work
- **Thread**: durable container for ongoing session, supports reconnection

The Thread is the durable state. Client can disconnect and reconnect. Agent keeps working.

**What they DON'T have:** Background jobs separate from agent. The agent IS the long-running process. Training a model would be the agent running `python train.py` as a shell command and waiting.

### Devin (Cognition)

**Architecture:** Each agent runs in a persistent VM. The VM stays alive for hours/days. Agent has full OS access. User connects/disconnects freely — agent continues working.

**Key insight:** Durability through persistent compute environment, not through workflow orchestration.

### Claude Code / Kiro CLI

**Architecture:** Local process, synchronous. Agent works while terminal is open. No background execution. No multi-agent.

**Limitation:** Cannot do anything that outlives the terminal session.

### Temporal / Durable Execution

**Architecture:** Workflow as code. Activities can be any duration. Framework handles retries, timeouts, state persistence. Workflow survives process crashes.

**Key insight:** Separate the workflow definition (what to do) from execution (how/where to run). State is automatically persisted at every step boundary.

### Synthesis

| Approach | Durability | Multi-agent | Async events | Complexity |
|----------|-----------|-------------|--------------|------------|
| Persistent process (Codex/Devin) | Process lifetime | Via delegation | Shell commands | Low |
| Durable workflow (Temporal) | Infinite | Via activities | Native | High |
| Event-driven state machine | Infinite | Via events | Native | Medium |

---

## 3. Our Design: Event-Sourced Durable Agent Loop

### Core Insight

We already have the building blocks:
- **conversation_events** — every action is already an event
- **Plans as events** — plan_created, plan_step_start, plan_step_done
- **Causal chains** — link everything together
- **Session persistence** — cross-session plan recovery already designed

The missing piece: **the agent loop itself needs to be event-driven and resumable**, not bound to a single request.

### Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│  AgentRun                                                       │
│                                                                 │
│  A durable unit of agent work. Survives HTTP disconnects,       │
│  process restarts, and async waits.                             │
│                                                                 │
│  States: pending → running → waiting → running → ... → done    │
│                                                                 │
│  Persisted as events in conversation_events.                    │
│  Resumed by replaying events + continuing from last state.      │
└─────────────────────────────────────────────────────────────────┘

User ──POST /chat──▶ Create AgentRun ──▶ Return run_id immediately
                          │
                          ▼
                    ┌─────────────┐
                    │  RunEngine  │ (async worker, not HTTP handler)
                    │             │
                    │  ChatLoop   │◀── same loop, but decoupled from HTTP
                    │  + Plan     │
                    │  + Tools    │
                    └──────┬──────┘
                           │
              ┌────────────┼────────────┐
              ▼            ▼            ▼
         In-process    Background    External
         tool call     Job submit    Event wait
         (instant)     (returns      (CI done,
                        job_id)      webhook)
                           │            │
                           └────────────┘
                                 │
                           ▼ (event arrives)
                    RunEngine resumes
```

### The Three Primitives

#### 1. AgentRun

A durable unit of work. Created when user sends a message that requires agent action.

```python
class RunStatus(str, Enum):
    PENDING = "pending"       # Created, not yet started
    RUNNING = "running"       # Agent loop actively executing
    WAITING = "waiting"       # Blocked on async event (job, webhook, approval)
    COMPLETED = "completed"   # Finished successfully
    FAILED = "failed"         # Finished with error
    CANCELLED = "cancelled"   # User cancelled

class AgentRun:
    run_id: str
    session_id: str
    user_id: str
    status: RunStatus
    
    # What triggered this run
    trigger: str              # "user_message" | "job_completed" | "webhook" | "schedule"
    trigger_event_id: str     # The event that started this run
    
    # What it's waiting for (when status == WAITING)
    waiting_for: str | None   # "job:<job_id>" | "approval:<event_id>" | "webhook:<hook_id>"
    
    # Plan state (persisted, survives restart)
    plan_id: str | None       # Current plan being executed
    current_step: str | None  # Current step in plan
```

**Not a new table.** AgentRun is stored as events in conversation_events:
```
event_type = "run_started"    → metadata = {run_id, trigger, ...}
event_type = "run_waiting"    → metadata = {run_id, waiting_for, ...}
event_type = "run_resumed"    → metadata = {run_id, resumed_by, ...}
event_type = "run_completed"  → metadata = {run_id, ...}
```

#### 2. RunEngine

The async worker that drives agent runs. Decoupled from HTTP.

```python
class RunEngine:
    """Drives AgentRun execution. Not bound to HTTP request lifecycle."""
    
    async def start_run(self, run: AgentRun) -> None:
        """Start or resume an agent run."""
        
        # 1. Restore state from events (if resuming)
        state = self._restore_state(run)
        
        # 2. Build ChatLoop with restored context
        loop = self._build_chat_loop(run, state)
        
        # 3. Execute — same PAOR loop, but with async-aware tool execution
        async for event in loop.run_step_stream(...):
            # Persist every event
            self._persist_event(event)
            # Stream to connected clients (if any)
            self._broadcast(run.run_id, event)
            
            # Check if we need to wait
            if event.event_type == "run_waiting":
                # Park the run — will be resumed by event
                return
        
        # 4. Run completed
        self._mark_completed(run)
    
    async def resume_run(self, run_id: str, trigger_event: dict) -> None:
        """Resume a waiting run when its async event arrives."""
        run = self._load_run(run_id)
        run.status = RunStatus.RUNNING
        # Inject the trigger event result into the conversation
        self._inject_result(run, trigger_event)
        await self.start_run(run)
```

#### 3. AsyncTool

Tools that can return immediately with a "wait handle" instead of blocking.
Managed by `AsyncToolRegistry` — a singleton that ChatLoop and RunEngine both use.

```python
class AsyncToolRegistry:
    """Extensible registry for tools that park agent runs."""
    
    def register(name, executor, schema)   # Add new async tool type
    def is_async_tool(name) -> bool        # ChatLoop checks before execution
    def execute(name, params, run_id)      # Execute + auto-track handle→run
    def resolve_handle(handle) -> run_id   # Pop run_id waiting for handle
    def get_schemas() -> [openai_schema]   # Auto-injected into tools_schema

# Built-in: submit_job
async def submit_job(params, run_id) -> dict:
    job_id = await backend.submit(params["job_type"], ...)
    return {"job_id": job_id, "wait_for": f"job:{job_id}"}

# User-extensible: any framework
registry.register("start_workflow", temporal_executor, temporal_schema)
registry.register("wait_approval", approval_executor, approval_schema)
# On completion: engine.resolve_handle("workflow:<id>", result)
```

**Handle format:** `"<type>:<id>"` — pure convention, not parsed. Any string works.

**Flow:**
1. LLM calls async tool → `execute()` returns `{wait_for: "job:abc"}`
2. Registry auto-maps `"job:abc" → run_id`
3. ChatLoop yields StreamEvent with `wait_for` → RunEngine parks run
4. External event arrives → `resolve_handle("job:abc")` → returns run_id
5. RunEngine resumes run with result injected into context

### How It Handles the Scenarios

#### Scenario 1: "收集语料，训练模型，发布 v2"

```
User: "收集语料，训练模型，发布 v2"
  │
  ▼
AgentRun created (run_001)
  │
  ▼ PLAN
Plan: [collect_corpus → train_model → evaluate → publish]
  │
  ▼ ACT step 1
Tool: submit_job("corpus_collector", {...})
  → ToolResult(value="Job submitted: job_abc", wait_for="job:job_abc")
  │
  ▼ WAIT
AgentRun status → WAITING (waiting_for="job:job_abc")
HTTP response: {run_id: "run_001", status: "waiting", message: "语料收集中..."}
  │
  ... 30 minutes pass ...
  │
  ▼ EVENT: job:job_abc completed
RunEngine.resume_run("run_001", {result: {corpus_path: "/data/corpus_v2"}})
  │
  ▼ ACT step 2
Tool: submit_job("feedback_trainer", {corpus: "/data/corpus_v2"})
  → ToolResult(wait_for="job:job_def")
  │
  ▼ WAIT (again)
  ... 2 hours pass ...
  │
  ▼ EVENT: job:job_def completed
  │
  ▼ ACT step 3 (in-process, fast)
Tool: evaluate_model({old: "v1", new: "v2"})
  → ToolResult(value={accuracy: 0.94, improvement: "+3%"})
  │
  ▼ ACT step 4 (in-process, fast)
Tool: publish_model({version: "v2", metrics: {...}})
  │
  ▼ DONE
AgentRun status → COMPLETED
Notification → User: "模型 v2 已发布，准确率 94%（+3%）"
```

#### Scenario 2: "写代码，多角度 review，修改"

```
User: "实现 auth 模块"
  │
  ▼
AgentRun created (run_001, agent=code_agent)
  │
  ▼ PLAN
Plan: [implement → multi_review → revise → final_review]
  │
  ▼ ACT step 1: implement
Tool: write_code({...})  → immediate result (in-process)
  │
  ▼ ACT step 2: multi_review (fan-out)
Tool: request_review(agent="security_reviewer", code=...)
  → child AgentRun (run_002) created, wait_for="run:run_002"
Tool: request_review(agent="perf_reviewer", code=...)
  → child AgentRun (run_003) created, wait_for="run:run_003"
Tool: request_review(agent="style_reviewer", code=...)
  → child AgentRun (run_004) created, wait_for="run:run_004"
  │
  ▼ WAIT (waiting for all 3 child runs)
  │
  ... run_002, run_003, run_004 execute in parallel ...
  ... each is its own ChatLoop with its own LLM calls ...
  ... each reviewer writes review events to conversation_events ...
  │
  ▼ ALL child runs completed
RunEngine.resume_run("run_001", {
    reviews: [
        {agent: "security", issues: [...]},
        {agent: "perf", issues: [...]},
        {agent: "style", issues: [...]},
    ]
})
  │
  ▼ REFLECT
LLM synthesizes reviews, decides what to fix
  │
  ▼ ACT step 3: revise
Tool: write_code({fixes: [...]})  → immediate
  │
  ▼ ACT step 4: final_review (optional, based on severity)
  │
  ▼ DONE
```

#### Scenario 3: Scheduled / Event-Triggered

```
# Cron trigger: daily drift check
Schedule: "0 9 * * *" → create AgentRun(trigger="schedule", agent="drift_agent")

# Webhook trigger: CI completed
POST /webhooks/ci → create AgentRun(trigger="webhook", agent="ci_agent")

# Job completion trigger: resume waiting run
Job job_abc completed → RunEngine.resume_run(waiting_run_id)
```

---

## 4. Client Interaction Model

### Async-First API

```
POST /chat                    → {run_id, status: "running"}  (returns immediately)
GET  /chat/runs/{run_id}      → {status, events, progress}   (poll)
GET  /chat/runs/{run_id}/stream → SSE stream                 (live updates)
DELETE /chat/runs/{run_id}    → cancel
```

**Backward compatible:** For simple queries (no planning needed), the run completes within the same HTTP request. Client sees no difference. The `/chat` endpoint can still return the final message synchronously for fast queries.

```python
# Smart response: sync for fast, async for complex
@router.post("/chat")
async def chat(request: ChatRequest, ...):
    run = create_run(request)
    
    # Fast path: simple query, complete within timeout
    result = await engine.start_run_with_timeout(run, timeout=30)
    if result.status == RunStatus.COMPLETED:
        return ChatResponse(run_id=run.run_id, message=result.message)
    
    # Slow path: complex task, return run_id for polling/streaming
    return ChatResponse(run_id=run.run_id, status="running", 
                        message="任务已开始，可通过 run_id 查询进度")
```

### SSE Stream Reconnection

Client disconnects → reconnects with `Last-Event-ID` → server replays missed events from conversation_events → continues streaming.

```
GET /chat/runs/{run_id}/stream
Headers: Last-Event-ID: evt_42

→ Server replays evt_43, evt_44, ... from DB
→ Then continues live streaming
```

This is possible because **every event is already persisted to conversation_events**. Reconnection is just a DB query.

---

## 5. Multi-Agent: Runs as the Coordination Primitive

### Child Runs Replace Synchronous Delegation

Current `delegate_task` blocks the parent ChatLoop. With AgentRun:

```python
# Old: synchronous, blocks parent
result = await delegate_task(agent="reviewer", task="Review this code")

# New: async, parent can fan-out and wait
review_run = engine.create_child_run(agent="reviewer", task="Review this code")
# Parent run parks, child run executes independently
# When child completes → parent resumes with result
```

### Fan-Out / Fan-In

```python
# Parent agent decides to fan out
child_runs = []
for reviewer in ["security", "perf", "style"]:
    child = engine.create_child_run(agent=reviewer, task=f"Review: {code}")
    child_runs.append(child)

# Return async wait for ALL children
return ToolResult(
    value=f"Requested {len(child_runs)} reviews",
    wait_for=[f"run:{c.run_id}" for c in child_runs],  # Wait for all
)
```

### Adversarial Loop

```python
# Implement → Review → Revise → Review → ... until approved
for iteration in range(max_iterations):
    code = agent.write_code(task)
    review = await engine.create_child_run(agent="reviewer", task=f"Review: {code}")
    # Parent waits for reviewer
    if review.result.approved:
        break
    # Not approved → revise based on feedback
    task = f"Revise based on: {review.result.feedback}"
```

### Visibility

All coordination is visible in conversation_events:
```sql
-- See all runs in a session
SELECT * FROM conversation_events 
WHERE session_id = ? AND event_type LIKE 'run_%'
ORDER BY created_at;

-- See parent-child relationships
SELECT * FROM conversation_events
WHERE metadata->>'parent_run_id' = 'run_001';

-- See what a run is waiting for
SELECT * FROM conversation_events
WHERE event_type = 'run_waiting' AND metadata->>'run_id' = 'run_001';
```

---

## 6. State Recovery

### Event Sourcing = Free Durability

AgentRun state is reconstructed by replaying events:

```python
def _restore_state(self, run: AgentRun) -> RunState:
    events = db.query(
        "SELECT * FROM conversation_events "
        "WHERE metadata->>'run_id' = :run_id ORDER BY created_at",
        {"run_id": run.run_id}
    )
    
    state = RunState()
    for event in events:
        match event.event_type:
            case "run_started": state.status = "running"
            case "plan_created": state.plan = Plan.from_json(event.content)
            case "plan_step_done": state.mark_step_done(event.metadata["step_id"])
            case "run_waiting": state.waiting_for = event.metadata["waiting_for"]
            case "tool_result": state.add_tool_result(event)
    
    return state
```

**Process crash → restart → replay events → continue from last step.** No special checkpointing needed. This is the advantage of our event-centric architecture — durability is a natural consequence, not an add-on.

### Context Reconstruction for LLM

When resuming, the LLM needs conversation history. This is already solved:
1. conversation_events has all messages and tool results
2. Context snapshots capture the full state at any point
3. LLM messages are rebuilt from events (same as replay)

---

## 7. What Changes, What Doesn't

### Doesn't Change
- **ChatLoop** — same PAOR loop, same tool execution, same streaming
- **Skill/Tool execution** — still in-process function calls
- **MCP Bridge** — still process-level isolation via stdio/HTTP
- **ToolMockingLayer** — still handles replay/audit
- **conversation_events schema** — no new tables, just new event_types
- **SkillRequirement** — no execution resource fields

### Changes
- **`/chat` endpoint** — returns `run_id`, supports async
- **New: RunEngine** — async worker that drives AgentRun execution
- **New: AgentRun** — durable run state as events
- **New: AsyncTool protocol** — tools can return `wait_for` instead of blocking
- **New: Run resume** — job completion / webhook / child run triggers resume
- **New: `/chat/runs/{run_id}`** — status, stream, cancel endpoints
- **Planner** — plan state persisted per-run, not per-request

### New Event Types
```
run_started       — AgentRun begins
run_waiting       — AgentRun parks (waiting for async event)
run_resumed       — AgentRun resumes (async event arrived)
run_completed     — AgentRun finished
run_failed        — AgentRun failed
run_cancelled     — AgentRun cancelled
child_run_created — Parent created a child run
```

---

## 8. Implementation Phases

### Phase 1: Durable Run (foundation) ✅
- `AgentRun` as events in conversation_events
- `RunEngine` with start/resume/cancel/stream/restore
- `/chat` returns run_id (async-only, no sync fast-path)
- `/chat/runs/{run_id}` status + stream + cancel endpoints
- SSE reconnection via `last_index`

### Phase 2: Async Tools + Workflow Engine ✅
- `AsyncToolRegistry` — extensible registry for tools that park runs
- `wait_for` handle protocol: any `"<type>:<id>"` string
- `resolve_handle()` — unified resume mechanism for any handle type
- `submit_job` as built-in async tool
- Job completion → `on_job_completed` → `resolve_handle("job:<id>")` → resume
- `POST /jobs/webhook` endpoint for external job completion callbacks
- `LocalJobBackend.on_completed` callback for in-process job completion
- Resume injects async result into `user_input` so LLM sees what happened

**Self-built Workflow Engine** (`core/workflow/engine.py`):
- JSON-serializable DSL (Pydantic) — LLM can generate workflow definitions directly
- Step types: `job`, `parallel` (fan-out/fan-in), `condition` (safe_eval), `wait` (parks for external event), `workflow` (nested), `loop` (repeat until)
- Step-level retry with exponential backoff, workflow-level timeout
- Data flow via `inputs_from` (step output → next step input)
- Condition jumps skip intermediate steps (marks SKIPPED)
- Safe expression evaluator: regex-based, no eval/exec, resolves `steps.step_id.field` paths
- Full state serialization — `WorkflowRun` is JSON-serializable for DB persistence

**Workflow Lifecycle:**
- `submit_workflow` tool → creates `WorkflowDefinition` + `WorkflowRun` in DB → executes in background
- If workflow hits a `wait` step → parks, stores `waiting_for` handle in `_workflow_waits`
- External event → `resume_workflow(handle, result)` → continues execution → persists state
- On completion → `_resolve_workflow()` → resumes parent agent run with workflow result
- Crash recovery: `restore_waiting_workflows()` on startup, loads from DB
- Stale cleanup: `cleanup_stale_workflows()` runs hourly, fails workflows stuck >24h

**API Endpoints:**
- `GET /workflows` — list registered workflow definitions
- `GET /workflows/runs/{run_id}` — workflow run status + step results
- `POST /workflows/runs/{run_id}/resolve` — resolve a wait step (human approval, external event)

**Extensibility:** New async tool types require only:
```python
reg = get_async_tool_registry()
reg.register("start_workflow", my_executor, my_schema)
# On completion: engine.resolve_handle("workflow:<id>", result)
```
ChatLoop and RunEngine require zero changes.

**Built-in async tools:**
- `submit_job` — single background job, parks until job completes
- `submit_workflow` — multi-step workflow with branching/parallel/wait/loop, parks until entire workflow completes

**Integration examples (zero core changes):**
```python
# Celery
async def celery_exec(params, run_id=None):
    r = celery_app.send_task(params["task_name"], args=params.get("args", []))
    return {"task_id": r.id, "wait_for": f"celery:{r.id}"}
reg.register("celery_task", celery_exec, celery_schema)

# Temporal workflow
async def temporal_exec(params, run_id=None):
    handle = await client.start_workflow(params["workflow"], ...)
    return {"workflow_id": handle.id, "wait_for": f"temporal:{handle.id}"}
reg.register("start_workflow", temporal_exec, temporal_schema)

# Airflow DAG
async def airflow_exec(params, run_id=None):
    run = trigger_dag(params["dag_id"], conf=params.get("conf", {}))
    return {"dag_run_id": run.run_id, "wait_for": f"airflow:{run.run_id}"}
reg.register("trigger_airflow", airflow_exec, airflow_schema)
```

### Phase 3: Multi-Agent Runs
- Child run creation
- Fan-out / fan-in
- Adversarial review loop

### Phase 4: Triggers
- Webhook → run creation
- Schedule (cron) → run creation
- Event-driven triggers (drift threshold → auto-retrain)

---

## 9. Distributed Safety

> **Status**: Implemented — all coordination through DB, no cross-worker in-memory dependencies

### Problem

Phase 1-2 used in-memory dicts (`_active_runs`, `_run_events`, `_workflow_runs`, `_workflow_waits`) for run state. This works for single-worker but breaks with multiple workers behind a load balancer:

| Operation | Failure mode |
|-----------|-------------|
| SSE streaming | Client reconnects to worker B, but events are in worker A's memory |
| Resume run | Webhook hits worker B, but waiting run is on worker A |
| Cancel run | User cancels on worker B, but run is executing on worker A |
| Resume workflow | Job callback hits worker B, but workflow state is on worker A |
| Concurrent resume | Two webhooks arrive simultaneously, both workers try to resume |

### Solution: DB as Sole Coordination Layer

```
Worker A                    DB (MatrixOne)                Worker B
   │                            │                            │
   │── INSERT run_events ──────▶│                            │
   │                            │◀── SELECT run_events ──────│  (SSE stream)
   │                            │                            │
   │── UPDATE status=waiting ──▶│                            │
   │                            │◀── SELECT waiting runs ────│  (resume)
   │                            │                            │
   │                            │◀── INSERT resume_claim ────│  (optimistic lock)
   │                            │    IntegrityError? → skip  │
```

### Implementation Details

#### 1. Event Persistence (`run_events` table)

Every SSE event is dual-written: local buffer (zero-latency for same-worker clients) + DB INSERT (cross-worker access).

```sql
CREATE TABLE run_events (
    id          INT AUTO_INCREMENT PRIMARY KEY,
    run_id      VARCHAR(255) NOT NULL,
    idx         INT NOT NULL,              -- sequential within run; -1 = resume_claim
    event_type  VARCHAR(64) NOT NULL,
    data        JSON NOT NULL,
    event_id    VARCHAR(255),
    agent_id    VARCHAR(255),
    created_at  DATETIME DEFAULT NOW(),
    UNIQUE KEY uq_run_event_run_idx (run_id, idx)
);
```

Streaming path:
- Same worker: read from `_run_events` dict (instant)
- Cross worker: poll `run_events` table every 0.1s

#### 2. Optimistic Lock for Resume (`_try_claim_resume`)

Multiple workers may receive the same job-completion webhook. Only one should resume the run.

```
INSERT INTO run_events (run_id, idx, event_type, data)
VALUES (:run_id, -1, 'resume_claim', :data)
```

- `UNIQUE(run_id, idx)` ensures only one `idx=-1` row per run
- First INSERT succeeds → worker wins the claim
- Second INSERT → `IntegrityError` → worker skips
- DB unavailable → fallback `return True` (single-worker degradation)

#### 3. Cross-Worker Cancel

```
cancel_run(run_id):
  1. Set local status → CANCELLED
  2. Cancel asyncio task (if local)
  3. Cancel workflow (if local): engine.cancel(wf_name)
  4. Write to DB: UPDATE workflow_runs SET status='cancelled'

WorkflowEngine._execute_loop() — before each step:
  1. Check _cancelled set (local cancel)
  2. Check _is_cancelled_in_db() (remote cancel via DB query)
```

#### 4. Workflow Resume with DB Fallback

```
resume_workflow(handle, result):
  1. _workflow_waits[handle] → wf_id          (local memory)
  2. Miss → query workflow_runs WHERE waiting_for=handle  (DB fallback)
  3. _workflow_runs[wf_id] → entry            (local memory)
  4. Miss → _restore_workflow_entry(wf_id)    (reconstruct from DB)
  5. Resume workflow execution
```

#### 5. Run Restore from Events

```
restore_run(run_id):
  SELECT event_type, content, metadata FROM conversation_events
  WHERE metadata->>'run_id' = :run_id ORDER BY created_at

  Replay: run_started → run_waiting → ... → reconstruct AgentRun state
```

### Failure Modes & Degradation

| Scenario | Behavior |
|----------|----------|
| DB down during event persist | Non-fatal: local buffer still works, log warning |
| DB down during claim | Allow resume (single-worker fallback) |
| DB down during cancel check | Skip check (local cancel still works) |
| Worker crash mid-run | Run stays RUNNING; `restore_waiting_workflows()` on startup recovers WAITING runs; stuck RUNNING runs cleaned by hourly `cleanup_stale_workflows()` |

---

## 10. Why This Design

### vs. Temporal/Durable Execution
Temporal is powerful but adds a heavy dependency and requires learning a new programming model. Our approach uses what we already have (events + ChatLoop) and adds durability naturally through event sourcing. If we outgrow this, migrating to Temporal is straightforward because our events map cleanly to Temporal activities.

### vs. Persistent Process (Codex/Devin)
Persistent processes are simple but expensive (one VM per agent) and don't scale. Our approach is serverless-friendly: runs are parked when waiting, consuming zero compute. Resume on event.

### vs. Simple Job Queue
A job queue (Celery/Bull) handles background tasks but doesn't handle the agent decision loop — the LLM needs to reason about results, revise plans, coordinate agents. That's not a job, it's a stateful workflow.

### Our Innovation
**Event-sourced agent runs on an HTAP database with time-travel.** No other agent framework has this combination:
- Durability without Temporal (events ARE the state)
- Replay without special infrastructure (same events, same replay)
- Time-travel debugging (MatrixOne time-travel on run events)
- Zero-cost branching for "what-if" run exploration
- Multi-agent coordination through the same event blackboard
- Audit trail is automatic (every run action is an event)
