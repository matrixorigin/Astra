# Agent Loop Reliability

> **Status**: Core Design  
> **Last Updated**: 2026-03-04  
> **Supersedes**: Previous version of this document (same date, patch-based approach)  
> **Triggered By**: Session `019cb449` post-mortem  
> **Related**: [context-window-management.md](context-window-management.md), [skills-and-tools.md](skills-and-tools.md), [agents-and-orchestration.md](agents-and-orchestration.md)  
> **Review Incorporated**: 2026-03-04 — pluggable stages, circuit breaker cooldown, parallel tool execution, observability, self-learning router feedback loop

---

## The Real Problem

Session `019cb449` is a symptom. The disease is **ChatLoop's architecture**.

ChatLoop is a 1200-line file with two near-identical code paths (`run_step` ~200 lines, `run_step_stream` ~400 lines) that share no execution logic. Tool execution is duplicated across three locations. State is implicit — tracked via local variables and the message chain. There is no structured place to inject routing, failure detection, or budget enforcement without invasive surgery on both paths.

Adding a circuit breaker, a task router, and eager compression as external modules that "compose around" ChatLoop is the wrong approach. It creates four new modules that each need to hook into two duplicated code paths at multiple points. The result is more complexity, not less.

**The right fix is to restructure ChatLoop itself** — extract a shared execution pipeline that both `run_step` and `run_step_stream` use, with explicit state that routing, failure detection, and compression can operate on.

---

## Design Principles

1. **One execution path, two output modes.** The tool loop logic exists once. Streaming vs. non-streaming is an output concern, not an execution concern.
2. **Explicit execution state.** Every round has a typed state object: tools available, tools blocked, failures recorded, tokens consumed, rounds used. No implicit state in local variables.
3. **Pipeline stages, not external patches.** Routing, failure detection, and compression are stages within the execution pipeline, not modules bolted on from outside.
4. **Pluggable stage protocol.** Each stage conforms to `PipelineStage(Protocol)` so future stages (Planning, Verification, Human-in-loop) can be appended without modifying the pipeline core.
5. **Existing subsystems are good.** `tool_output_handler.py` (structured summaries, memory pointers, dynamic thresholds) and `compaction.py` (history compression) are well-designed. The problem is ChatLoop's integration of them, not the subsystems themselves.
6. **Observable by default.** Every stage emits structured events with round, blocked_tools, tokens — enabling Prometheus/Grafana dashboards for production monitoring.

---

## Architecture: Execution Pipeline

```
User Query
    │
    ▼
┌─────────────────────────────────────────────────────────────────┐
│                     ChatLoop (thin shell)                       │
│                                                                 │
│  run_step()        → pipeline.execute() → collect final text    │
│  run_step_stream() → pipeline.execute() → yield StreamEvents    │
│                                                                 │
└──────────────────────────┬──────────────────────────────────────┘
                           │
                           ▼
┌─────────────────────────────────────────────────────────────────┐
│                   ExecutionPipeline                              │
│                                                                 │
│  State: TurnState (mutable, passed through all stages)          │
│                                                                 │
│  ┌───────────┐  ┌───────────┐  ┌───────────┐  ┌─────────────┐  │
│  │  ROUTE    │→ │  CALL LLM │→ │  EXECUTE  │→ │  EVALUATE   │  │
│  │           │  │           │  │  TOOLS    │  │             │  │
│  │ Classify  │  │ Compact   │  │ Per-tool: │  │ Check:      │  │
│  │ intent,   │  │ history,  │  │ breaker → │  │ breaker     │  │
│  │ scope     │  │ resolve   │  │ execute → │  │ state,      │  │
│  │ tools,    │  │ model,    │  │ compress  │  │ budget,     │  │
│  │ set       │  │ call LLM  │  │ output →  │  │ progress    │  │
│  │ budget    │  │           │  │ append    │  │ → continue  │  │
│  │           │  │           │  │           │  │   or exit   │  │
│  └───────────┘  └───────────┘  └───────────┘  └─────────────┘  │
│       ↑                                              │          │
│       └──────────────── loop ────────────────────────┘          │
│                                                                 │
│  Future stages (append to pipeline, zero modification):         │
│  ┌───────────┐  ┌───────────┐  ┌───────────┐                   │
│  │ VERIFY    │  │ PLAN      │  │ HUMAN     │                   │
│  │ (firewall)│  │ (decomp)  │  │ (approval)│                   │
│  └───────────┘  └───────────┘  └───────────┘                   │
│                                                                 │
└─────────────────────────────────────────────────────────────────┘
```

### Pluggable Stage Protocol

The pipeline is a `list[PipelineStage]`. The execution engine iterates stages — it never knows what's inside them. Adding a stage = appending to the list. Removing a stage = removing from the list. Zero modification to the engine.

```python
class PipelineStage(Protocol):
    """A single stage in the execution pipeline.
    
    Mutates state in place. Sets state.outcome to signal early exit.
    Returns an async iterator of TurnEvents for streaming.
    """
    def __call__(self, state: TurnState) -> AsyncIterator[TurnEvent]: ...

@dataclass
class ExecutionPipeline:
    """The engine. Runs stages in order, loops when needed."""
    pre_loop: list[PipelineStage]    # Run once before tool loop (e.g. route)
    loop_body: list[PipelineStage]   # Run each round (e.g. call_llm, execute_tools, evaluate)
    post_loop: list[PipelineStage]   # Run once after loop exits (e.g. final_answer)

# Default pipeline configuration:
pipeline = ExecutionPipeline(
    pre_loop=[RouteStage(classify_intent)],
    loop_body=[CallLLMStage(llm), ExecuteToolsStage(executor), EvaluateStage()],
    post_loop=[FinalAnswerStage(llm)],
)

# Adding verification — zero invasion:
pipeline.loop_body.insert(3, VerificationStage(firewall))

# Adding human-in-loop — zero invasion:
pipeline.loop_body.append(HumanApprovalStage(hitl_policy))

# Adding planning — zero invasion:
pipeline.pre_loop.insert(0, PlanningStage(planner))
```

Each stage is a class with injected dependencies (LLM client, executor, etc.), not a bare function. This makes them independently testable and configurable.

### TurnState: The Execution State Object

This is the key structural change. Instead of implicit state scattered across local variables, every round operates on a single typed state object.

```python
@dataclass
class TurnState:
    """Mutable state for one user turn, passed through all pipeline stages."""

    # --- Identity ---
    session_id: str
    user_id: str
    user_query: str
    user_event: ConversationEvent
    context_capture_id: str

    # --- Messages ---
    messages: list[dict[str, Any]]       # The LLM message chain
    tools_schema: list[dict[str, Any]]   # Available tools (mutable — routing can filter)

    # --- Execution tracking ---
    round: int = 0
    max_rounds: int = MAX_TOOL_ROUNDS
    last_skill_name: str | None = None

    # --- Circuit breaker ---
    tool_failures: dict[str, list[str]] = field(default_factory=dict)  # tool → [error_msgs]
    blocked_tools: set[str] = field(default_factory=set)

    # --- Budget ---
    tokens_consumed: int = 0
    tool_output_bytes: int = 0
    wall_clock_start: float = field(default_factory=time.monotonic)

    # --- Outcome ---
    outcome: TurnOutcome | None = None   # Set when turn completes or fails
```

```python
@dataclass
class TurnOutcome:
    """How the turn ended."""
    status: Literal["success", "failure", "exhausted"]
    content: str                          # Final text to deliver
    failure_reason: str | None = None     # If status == "failure"
    failed_tools: list[str] = field(default_factory=list)
    verification: Any = None              # Firewall result
```

### Why TurnState Changes Everything

With TurnState, the four problems from session 019cb449 become trivial to solve:

1. **Routing**: The ROUTE stage sets `state.tools_schema` and `state.max_rounds` based on intent classification. No separate module needed — it's just the first stage of the pipeline.

2. **Circuit breaker**: The EXECUTE stage updates `state.tool_failures` and `state.blocked_tools` after each tool call. The EVALUATE stage checks them. No separate module — it's state transitions on TurnState.

3. **Output compression**: The EXECUTE stage calls `tool_output_handler.process_tool_output()` unconditionally (not gated on `_memory_store`). Already implemented — just needs the guard removed.

4. **Failure reporting**: The EVALUATE stage checks `state.blocked_tools`, `state.round`, `state.tokens_consumed`. If the turn is failing, it sets `state.outcome` with a structured failure report. The loop exits.

---

## Stage 1: ROUTE → `RouteStage`

Pre-loop stage. Runs once per turn, before the tool loop.

```python
def route(state: TurnState) -> None:
    """Classify intent and scope tools. Mutates state in place."""
    classification = classify_intent(state.user_query)

    if classification.confidence > ROUTING_THRESHOLD:
        # Filter tools
        if classification.blocked_tools:
            blocked = set(classification.blocked_tools)
            state.tools_schema = [
                t for t in state.tools_schema
                if t["function"]["name"] not in blocked
            ]
        # Override max rounds
        if classification.max_rounds is not None:
            state.max_rounds = classification.max_rounds
```

**Intent classification** — same keyword + embedding hybrid as before, but simpler because it only needs to answer one question: "should I restrict the tool set?"

| Intent | Restriction | Max Rounds |
|--------|------------|------------|
| External data fetch (github, api, 最新, issue) | Block local-only tools | 3 |
| Pure conversation (what is, explain) | Block all tools | 0 |
| Everything else | No restriction | 10 |

Only two categories that restrict. Everything else passes through unchanged. This is deliberately minimal — the self-improving selector can learn more categories over time.

**Relationship to ModernSkillSelector**: The router operates on the tool schema list *after* ModernSkillSelector has already selected candidates. It's a post-filter, not a replacement. ModernSkillSelector does semantic retrieval; the router does intent-based scoping.

---

## Stage 2: CALL LLM → `CallLLMStage`

Loop body stage. Runs each round. Handles compaction and LLM invocation.

```python
async def call_llm(state: TurnState, llm, tools_schema) -> dict:
    """Compact history if needed, call LLM, return result."""
    # Compaction — uses existing compaction.py, unchanged
    state.messages = compact_history_messages(state.messages)
    if needs_compaction(state.messages, max_tokens):
        state.messages = compact(state.messages, max_tokens)

    return await llm.chat_with_tools(
        messages=state.messages,
        tools=active_tools(state),  # Exclude blocked tools
        tool_choice="auto",
        model=resolve_model(...),
    )

def active_tools(state: TurnState) -> list[dict]:
    """Tools minus blocked ones."""
    return [t for t in state.tools_schema
            if t["function"]["name"] not in state.blocked_tools]
```

No new logic here. Just existing compaction + LLM call, but operating on TurnState instead of local variables.

---

## Stage 3: EXECUTE TOOLS → `ExecuteToolsStage`

Loop body stage. Runs for each tool call in a round (in parallel via `asyncio.gather`). This is where the circuit breaker and output compression live.

```python
async def execute_tool(state: TurnState, tc: dict, executor, memory_store) -> str:
    """Execute one tool call with breaker check and output compression."""
    fn_name = tc["function"]["name"]

    # --- Breaker check (pre-execution) ---
    if fn_name in state.blocked_tools:
        return json.dumps({
            "error": f"Tool '{fn_name}' is unavailable (failed previously)",
            "blocked": True,
            "suggestion": "Use a different approach or inform the user.",
        })

    # --- Execute ---
    try:
        result_str = await _dispatch_tool(executor, fn_name, tc, state)
        # Success: clear failure history for this tool
        state.tool_failures.pop(fn_name, None)
    except Exception as e:
        result_str = json.dumps({"error": str(e)})
        # Record failure
        state.tool_failures.setdefault(fn_name, []).append(str(e))
        # Check breaker threshold
        if _should_break(state.tool_failures[fn_name]):
            state.blocked_tools.add(fn_name)
            result_str = json.dumps({
                "error": str(e),
                "blocked": True,
                "message": f"Tool '{fn_name}' disabled after repeated failures. "
                           "Do NOT retry. Inform the user.",
            })

    # --- Compress output (unconditional — this is the key change) ---
    result_str = process_tool_output(
        output=result_str,
        tool_name=fn_name,
        session_id=state.session_id,
        user_id=state.user_id,
        memory_store=memory_store,       # May be None — handler already handles this
        turn_event_id=state.user_event.event_id,
        remaining_tokens=_remaining_budget(state),
    )

    state.tool_output_bytes += len(result_str)
    state.last_skill_name = fn_name
    return result_str


def _should_break(failures: list[str]) -> bool:
    """Should this tool be circuit-broken?"""
    if len(failures) >= 3:
        return True  # 3 failures of any kind
    if len(failures) >= 2:
        # 2 failures with similar error → break
        return _errors_similar(failures[-2], failures[-1])
    return False


def _errors_similar(a: str, b: str) -> bool:
    """Two errors are similar if they share >60% of words."""
    wa, wb = set(a.lower().split()), set(b.lower().split())
    overlap = len(wa & wb)
    return overlap / max(len(wa), 1) > 0.6
```

**Circuit breaker cooldown (Phase 1 enhancement)**:

Within a single turn, blocked tools stay blocked (no recovery — the turn is short-lived). Across sessions, the breaker uses exponential cooldown to allow recovery from transient failures:

```python
@dataclass
class BreakerRecord:
    """Persisted per (user_id, tool_name). Stored in DB."""
    consecutive_failures: int = 0
    last_failure_at: datetime | None = None
    cooldown_until: datetime | None = None  # None = not cooling down

COOLDOWN_SCHEDULE = [timedelta(minutes=5), timedelta(minutes=30), timedelta(hours=2)]

def update_breaker(record: BreakerRecord, failed: bool) -> BreakerRecord:
    if not failed:
        record.consecutive_failures = 0
        record.cooldown_until = None
        return record
    record.consecutive_failures += 1
    record.last_failure_at = datetime.now(timezone.utc)
    idx = min(record.consecutive_failures - 1, len(COOLDOWN_SCHEDULE) - 1)
    record.cooldown_until = record.last_failure_at + COOLDOWN_SCHEDULE[idx]
    return record
```

This converts "sandbox network flicker" from permanent failure to automatic recovery. The `BreakerRecord` is persisted at user_id level — if a tool fails for user A, it doesn't affect user B. Implementation deferred to Phase 4 (requires DB schema for `tool_breaker_state` table).

**Key change**: `process_tool_output()` is called unconditionally. Currently ChatLoop gates it on `_memory_store` being available. The handler already has fallback behavior when memory_store is None (it truncates). We just remove the guard.

**No new compression module.** The existing `tool_output_handler.py` already does:
- Dynamic threshold based on remaining budget ✓
- Per-tool structured summaries (grep, shell, list_dir, JSON) ✓
- Memory pointer for large outputs ✓
- `memory_expand` tool for LLM to access full content ✓

All we need is to call it unconditionally.

---

## Stage 4: EVALUATE → `EvaluateStage`

Loop body stage. Runs after all tools in a round are executed. Decides: continue loop, or exit.

```python
def evaluate(state: TurnState) -> Literal["continue", "exit"]:
    """Check if the turn should continue or exit."""
    state.round += 1

    # All tools blocked → task failure
    if state.blocked_tools and not active_tools(state):
        state.outcome = TurnOutcome(
            status="failure",
            content=_build_failure_report(state),
            failure_reason="all_tools_blocked",
            failed_tools=list(state.blocked_tools),
        )
        return "exit"

    # Critical tools blocked → task failure
    # (if the only tools left are scratchpad/reflect, the task can't progress)
    remaining = {t["function"]["name"] for t in active_tools(state)}
    productive = remaining - {"scratchpad_write", "scratchpad_read", "scratchpad_close", "reflect"}
    if not productive and state.blocked_tools:
        state.outcome = TurnOutcome(
            status="failure",
            content=_build_failure_report(state),
            failure_reason="no_productive_tools",
            failed_tools=list(state.blocked_tools),
        )
        return "exit"

    # Round limit
    if state.round >= state.max_rounds:
        return "exit"  # Existing behavior: ask LLM for final answer

    # Token budget (optional — if budget manager available)
    elapsed = time.monotonic() - state.wall_clock_start
    if elapsed > WALL_CLOCK_TIMEOUT:
        state.outcome = TurnOutcome(
            status="failure",
            content="⚠️ Task timed out. Please try a simpler query.",
            failure_reason="timeout",
        )
        return "exit"

    return "continue"


def _build_failure_report(state: TurnState) -> str:
    """Build user-facing failure report from TurnState."""
    parts = ["⚠️ **Task cannot be completed**\n"]

    for tool_name in state.blocked_tools:
        errors = state.tool_failures.get(tool_name, [])
        last_error = errors[-1] if errors else "unknown"
        parts.append(f"- `{tool_name}` failed {len(errors)}× — {last_error[:200]}")

    parts.append("\n**Suggestion**: This environment may not support the required "
                 "capabilities. Try a different approach or check tool permissions.")
    return "\n".join(parts)
```

---

## The Unified Loop

The execution engine drives the pipeline. It knows nothing about routing, LLM calls, or tool execution — it just runs stages.

```python
async def _execute_turn(self, state: TurnState) -> AsyncIterator[TurnEvent]:
    """The unified execution engine. Runs pipeline stages, yields events."""

    # Pre-loop stages (run once)
    for stage in self.pipeline.pre_loop:
        async for event in stage(state):
            yield event
        if state.outcome:
            return  # Stage signaled early exit (e.g. CONVERSATIONAL → no tools)

    # Tool loop
    while state.round < state.max_rounds and not state.outcome:
        for stage in self.pipeline.loop_body:
            async for event in stage(state):
                yield event
            if state.outcome:
                return  # Stage signaled exit (success, failure, or blocked)

    # Post-loop stages (run once — e.g. final answer when rounds exhausted)
    if not state.outcome:
        for stage in self.pipeline.post_loop:
            async for event in stage(state):
                yield event
```

That's the entire engine — 18 lines. It never changes. All behavior lives in the stages.

### Stage Implementations (the 4 initial stages)

Each stage is a class that conforms to `PipelineStage`:

```python
@dataclass
class RouteStage:
    """Pre-loop: classify intent, scope tools, set budget."""
    classify: Callable[[str], IntentClassification]

    async def __call__(self, state: TurnState) -> AsyncIterator[TurnEvent]:
        classification = self.classify(state.user_query)
        if classification.blocked_tools:
            blocked = set(classification.blocked_tools)
            state.tools_schema = [
                t for t in state.tools_schema
                if t["function"]["name"] not in blocked
            ]
        if classification.max_rounds is not None:
            state.max_rounds = classification.max_rounds
        if not state.tools_schema or state.max_rounds == 0:
            state.outcome = TurnOutcome(status="success", content="")  # Will be filled by post_loop
        yield TurnEvent("route", {"intent": classification.intent, "tools": len(state.tools_schema)})


@dataclass
class CallLLMStage:
    """Loop body: compact history, call LLM, handle final answer."""
    llm: Any

    async def __call__(self, state: TurnState) -> AsyncIterator[TurnEvent]:
        state.messages = compact_history_messages(state.messages)
        llm_result = await self.llm.chat_with_tools(
            messages=state.messages,
            tools=active_tools(state),
            tool_choice="auto",
        )
        tool_calls = llm_result.get("tool_calls", [])
        if not tool_calls:
            state.outcome = TurnOutcome(status="success", content=llm_result.get("content", ""))
            yield TurnEvent("final_text", state.outcome.content)
            return
        state.messages.append({
            "role": "assistant",
            "content": llm_result.get("content", ""),
            "tool_calls": tool_calls,
        })
        state._pending_tool_calls = tool_calls  # Pass to next stage
        yield TurnEvent("llm_response", {"tool_calls": len(tool_calls)})


@dataclass
class ExecuteToolsStage:
    """Loop body: execute tool calls in parallel, compress output, update breaker."""
    executor: Any
    memory_store: Any = None

    async def __call__(self, state: TurnState) -> AsyncIterator[TurnEvent]:
        tool_calls = getattr(state, '_pending_tool_calls', [])
        if not tool_calls:
            return
        results = await asyncio.gather(
            *[execute_tool(state, tc, self.executor, self.memory_store)
              for tc in tool_calls],
            return_exceptions=True,
        )
        for tc, result in zip(tool_calls, results):
            if isinstance(result, Exception):
                result = json.dumps({"error": str(result)})
            state.messages.append({"role": "tool", "tool_call_id": tc["id"], "content": result})
            yield TurnEvent("tool_result", {"call_id": tc["id"], "result": result[:500]})
        state._pending_tool_calls = []


@dataclass
class EvaluateStage:
    """Loop body: check breaker state, budget, progress. Decide continue or exit."""

    async def __call__(self, state: TurnState) -> AsyncIterator[TurnEvent]:
        state.round += 1
        # All productive tools blocked → failure
        remaining = {t["function"]["name"] for t in active_tools(state)}
        productive = remaining - UTILITY_TOOLS
        if not productive and state.blocked_tools:
            state.outcome = TurnOutcome(
                status="failure",
                content=_build_failure_report(state),
                failure_reason="all_tools_blocked",
                failed_tools=list(state.blocked_tools),
            )
        # Timeout
        elif (time.monotonic() - state.wall_clock_start) > WALL_CLOCK_TIMEOUT:
            state.outcome = TurnOutcome(
                status="failure", content="⚠️ Task timed out.",
                failure_reason="timeout",
            )
        yield TurnEvent("evaluate", {
            "round": state.round, "blocked": list(state.blocked_tools),
            "decision": "exit" if state.outcome else "continue",
        })


@dataclass
class FinalAnswerStage:
    """Post-loop: when rounds exhausted without outcome, ask LLM for final answer."""
    llm: Any

    async def __call__(self, state: TurnState) -> AsyncIterator[TurnEvent]:
        if state.outcome:
            return  # Already have an outcome
        state.messages.append({"role": "system", "content": "Provide your final answer."})
        result = await self.llm.chat_with_tools(messages=state.messages, tools=[])
        state.outcome = TurnOutcome(status="exhausted", content=result.get("content", ""))
        yield TurnEvent("final_text", state.outcome.content)
```

Then the two public methods become thin wrappers:

```python
async def run_step(self, user_input, session_id, user_id, context=None, **kw) -> str:
    state = self._init_turn_state(user_input, session_id, user_id, context)
    async for event in self._execute_turn(state):
        pass  # Consume events, keep only final
    return state.outcome.content if state.outcome else ""

async def run_step_stream(self, user_input, session_id, user_id, context=None, **kw):
    state = self._init_turn_state(user_input, session_id, user_id, context)
    async for event in self._execute_turn(state):
        yield self._to_stream_event(event, state)  # Convert to StreamEvent
```

**This eliminates ~400 lines of duplicated code** between `run_step` and `run_step_stream`.

---

## What Changes vs. What Stays

| Component | Change | Rationale |
|-----------|--------|-----------|
| `ChatLoop.run_step` | **Rewrite** → thin wrapper over `_execute_turn` | Eliminate duplication |
| `ChatLoop.run_step_stream` | **Rewrite** → thin wrapper over `_execute_turn` | Eliminate duplication |
| `ChatLoop._execute_single_tool` | **Absorbed** into `ExecuteToolsStage` | Single tool execution path |
| `ChatLoop.run_step_with_planning` | **Unchanged** | PAOR loop is a separate concern |
| `tool_output_handler.py` | **Unchanged** | Already well-designed; just remove the guard |
| `compaction.py` | **Unchanged** | Already well-designed; called from `CallLLMStage` |
| `modern_selector.py` | **Unchanged** | `RouteStage` post-filters its output, doesn't replace it |
| `_build_messages()` | **Unchanged** | Prompt composition is orthogonal |
| Firewall verification | **Future stage** `VerificationStage` | Append to `loop_body`, zero invasion |
| Event logging | **Moved** into `_to_stream_event()` | Same events, single location |

**New code:**
- `TurnState` + `TurnOutcome` + `TurnEvent` dataclasses (~45 lines)
- `PipelineStage` Protocol + `ExecutionPipeline` dataclass (~15 lines)
- `RouteStage` class (~25 lines)
- `CallLLMStage` class (~30 lines)
- `ExecuteToolsStage` class (~35 lines, parallel via asyncio.gather)
- `EvaluateStage` class (~30 lines)
- `FinalAnswerStage` class (~15 lines)
- `_execute_turn()` engine (~18 lines — just iterates stages)
- `execute_tool()` helper with breaker (~40 lines)
- `_should_break()` + `_errors_similar()` + `_build_failure_report()` (~30 lines)
- Per-stage observability events (~20 lines)

**Net effect**: ~300 lines of new code replaces ~600 lines of duplicated code. ChatLoop shrinks from ~1200 lines to ~800 lines. The engine itself is 18 lines and never changes.

---

## How Session 019cb449 Plays Out

```
User: "matrixone最新的两个issue？"

ROUTE:
  Keywords "最新" + "issue" → external fetch intent
  Block: [list_dir, grep, read_file]  (local-only tools)
  Max rounds: 3
  tools_schema: [github_api, http_request, web_search, execute_code, shell]

Round 1:
  CALL LLM → tool_calls: [execute_code(requests.get(github_api...))]
  EXECUTE: execute_code fails (sandbox blocks os import)
    state.tool_failures["execute_code"] = ["os module blocked"]
  EVALUATE: 1 failure, continue

Round 2:
  CALL LLM → tool_calls: [execute_code(urllib...)]
  EXECUTE: execute_code fails again (sandbox blocks network)
    state.tool_failures["execute_code"] = ["os module blocked", "network blocked"]
    _errors_similar → True → state.blocked_tools.add("execute_code")
    Result injected: {"blocked": true, "message": "Do NOT retry..."}
  EVALUATE: execute_code blocked, but other tools remain, continue

Round 3:
  CALL LLM sees: execute_code is blocked, tries shell or gives up
  If shell also fails → blocked
  EVALUATE: no productive tools remain → EXIT with failure report

Output:
  ⚠️ Task cannot be completed
  - `execute_code` failed 2× — sandbox security restriction
  - `shell` failed 1× — network access denied
  Suggestion: This environment doesn't support external API calls.

Total: 3 rounds, ~15K tokens, ~20 seconds
(vs. original: 10+ rounds, 87K tokens, 116 seconds)
```

---

## Edge-Cloud Considerations

In edge-cloud mode (`agents-and-orchestration.md` §2), the edge drives the tool loop and cloud does LLM + context. TurnState needs to be serializable for the `/chat/turn` API boundary:

- Edge sends tool results → Cloud runs CALL LLM + ROUTE + EVALUATE
- Cloud returns: tool_calls + updated `blocked_tools` + `round` count
- Edge executes tools locally, sends results back

The `blocked_tools` and `tool_failures` fields in TurnState travel across the wire as part of the turn response. This is a natural extension — the existing `/chat/turn` API already returns tool_calls; we add `execution_state: {blocked_tools, round, max_rounds}` to the response.

**Serialization**: TurnState provides `to_wire()` / `from_wire()` that serialize only the fields needed for cross-boundary state: `blocked_tools`, `tool_failures`, `round`, `max_rounds`, `outcome`. Messages and tools_schema are NOT serialized (Cloud reconstructs them).

**Cloud-side validation**: Cloud enforces `max_rounds <= GLOBAL_MAX_ROUNDS` to prevent Edge from bypassing budget by sending inflated values. `blocked_tools` from Edge are additive-only — Cloud can add blocks but Edge cannot remove them.

---

## Observability

Every pipeline stage emits a structured event on completion. This enables Prometheus + Grafana dashboards for production monitoring.

```python
# Emitted at the end of each stage
logger.info("pipeline.stage_complete", extra={
    "stage": stage_name,           # "route" | "call_llm" | "execute_tools" | "evaluate"
    "round": state.round,
    "blocked_tools": list(state.blocked_tools),
    "tokens_consumed": state.tokens_consumed,
    "tool_output_bytes": state.tool_output_bytes,
    "elapsed_ms": int((time.monotonic() - stage_start) * 1000),
    "session_id": state.session_id,
})

# Emitted when a turn completes
logger.info("pipeline.turn_complete", extra={
    "outcome": state.outcome.status if state.outcome else "unknown",
    "total_rounds": state.round,
    "total_tokens": state.tokens_consumed,
    "blocked_tools": list(state.blocked_tools),
    "wall_clock_ms": int((time.monotonic() - state.wall_clock_start) * 1000),
    "failure_reason": state.outcome.failure_reason if state.outcome else None,
})
```

**Key metrics to dashboard**:
- Round distribution per session (histogram) — detect runaway loops
- Tool failure rate per tool (counter) — detect broken tools before users report
- Tokens per turn (histogram) — detect context bloat
- Circuit breaker activations (counter) — detect systemic tool issues
- Stage latency (histogram) — detect slow stages

These events are also persisted as `conversation_events` (type `pipeline_stage_complete`) for post-hoc analysis and replay.

---

## Self-Learning Router Feedback Loop

The intent router starts with keyword + embedding rules (Phase 2). Over time, it improves via a closed feedback loop:

```
Turn starts → ROUTE classifies intent → Turn executes → EVALUATE records outcome
                                                              │
                                                              ▼
                                                    Log: (query, classification, outcome)
                                                              │
                                                    Weekly batch job:
                                                    - Analyze misclassifications
                                                    - Generate updated rules (LLM-as-judge)
                                                    - Or fine-tune small classifier
                                                              │
                                                              ▼
                                                    Updated router weights/rules
```

**Phase 2 (immediate)**: Log every `(query, classification, actual_outcome)` triple as a `route_feedback` event. Zero cost, pure data collection.

**Phase 4 (later)**: Weekly batch job analyzes misclassifications. Two upgrade paths:
1. **Rule refinement**: LLM-as-judge reviews misclassified turns, generates new keyword rules
2. **Classifier fine-tune**: Train a small classifier on accumulated data (replaces keyword matching)

**Future**: ToolSEE-style vector retrieval + dynamic tool expansion, where the router doesn't just filter tools but actively discovers relevant ones from the full registry.

---

## Industry Alignment

| Capability | Current ChatLoop | Restructured ChatLoop | Industry SOTA |
|-----------|-----------------|----------------------|---------------|
| Single execution path | ❌ (2 duplicated paths) | ✅ `_execute_turn` | Standard practice |
| Explicit execution state | ❌ (local vars) | ✅ TurnState | LangGraph (state machine), Temporal (durable state) |
| Pluggable pipeline stages | ❌ | ✅ PipelineStage Protocol | LangGraph, CrewAI, AutoGen |
| Intent-based tool scoping | ❌ | ✅ ROUTE stage | ToolSEE, SkillMesh, OpenAI planner-executor |
| Per-tool circuit breaker | ❌ | ✅ In EXECUTE stage | InfoWorld FinOps guardrails |
| Breaker cooldown + recovery | ❌ | ✅ Exponential backoff | Standard microservice pattern |
| Parallel tool execution | ❌ (serial for-loop) | ✅ asyncio.gather | OpenAI, Anthropic parallel calls |
| Unconditional output compression | Partial (gated on memory_store) | ✅ Always-on | IBM Memory Pointer (arXiv 2511.22729) |
| Structured failure reporting | ❌ | ✅ EVALUATE stage | Not widely available |
| Budget enforcement | Partial (compaction) | ✅ TurnState tracking | Anthropic context engineering |
| Pipeline observability | ❌ | ✅ Per-stage structured events | Standard production practice |
| Self-learning routing | ❌ | ✅ Feedback loop (Phase 4) | ToolSEE, SkillMesh |

---

## References

1. IBM Research — Solving Context Window Overflow in AI Agents (arXiv 2511.22729)
2. InfoWorld — FinOps for Agents: Loop Limits, Tool-Call Caps (2026-03)
3. OpenAI — A Practical Guide to Building Agents (2026)
4. Anthropic — Effective Context Engineering for AI Agents (2025-2026)
5. LangChain — Context Engineering in Agents
6. ToolSEE — Agent Tool Search Engine (preprints.org 202512.1744)
7. ProductHunt — Lessons Learned from Building Agents (2026)
