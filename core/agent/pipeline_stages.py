"""Pipeline stages and execute_turn() engine.

Each stage is an async callable: (TurnState) -> AsyncIterator[TurnEvent].
The engine threads TurnState through pre_loop → loop_body (repeated) → post_loop.
"""

from __future__ import annotations

import asyncio
import json
import time
from collections.abc import AsyncIterator, Callable  # noqa: TC003 — runtime use in signatures

from core.agent.turn_state import (
    ExecutionPipeline,
    TurnEvent,
    TurnOutcome,
    TurnState,
    TurnStatus,
)
from core.context.intent_routing import LOCAL_TOOLS

from core.logging_config import get_logger

logger = get_logger(__name__)

# Breaker thresholds
_MAX_ANY_FAILURES = 3
_MAX_SIMILAR_FAILURES = 2
_SIMILARITY_THRESHOLD = 0.60


# ---------------------------------------------------------------------------
# Utility
# ---------------------------------------------------------------------------

def _errors_similar(a: str, b: str) -> bool:
    """Word-overlap similarity > threshold."""
    wa = set(a.lower().split())
    wb = set(b.lower().split())
    if not wa or not wb:
        return False
    overlap = len(wa & wb)
    return overlap / min(len(wa), len(wb)) > _SIMILARITY_THRESHOLD


def _should_break(failures: list[str]) -> bool:
    """Decide whether to circuit-break a tool.

    Break if:
    - 3+ failures of any kind, OR
    - 2+ failures with >60% word overlap (similar errors)
    """
    if len(failures) >= _MAX_ANY_FAILURES:
        return True
    if len(failures) >= _MAX_SIMILAR_FAILURES:
        # Check pairwise similarity of last N
        for i in range(len(failures)):
            for j in range(i + 1, len(failures)):
                if _errors_similar(failures[i], failures[j]):
                    return True
    return False


def _active_tools(state: TurnState) -> list[dict]:
    """Return tools_schema minus blocked tools."""
    return [
        t for t in state.tools_schema
        if t.get("function", {}).get("name") not in state.blocked_tools
    ]


def _wall_clock_exceeded(state: TurnState) -> bool:
    return (time.monotonic() - state.wall_clock_start) > state.wall_clock_timeout


# ---------------------------------------------------------------------------
# Stages
# ---------------------------------------------------------------------------

class RouteStage:
    """Pre-loop: classify intent, filter tools, set max_rounds.

    Accepts either a classify_intent callable (returns IntentClassification or str)
    or a RoutingDecision object directly.
    """

    def __init__(self, classify_intent: Callable | None = None):
        self._classify = classify_intent

    async def __call__(self, state: TurnState) -> AsyncIterator[TurnEvent]:
        if not self._classify:
            yield TurnEvent(event_type="stage_complete", data={"stage": "route", "classification": "DEFAULT"})
            return

        # Support both sync and async classify callables
        result = self._classify(state.user_input)
        if hasattr(result, "__await__"):
            result = await result

        # Handle RoutingDecision from unified router
        if hasattr(result, "tool_filter"):
            from core.context.intent_routing import ToolFilter
            if result.tool_filter == ToolFilter.ALL_BLOCKED:
                state.tools_schema = []
                state.max_rounds = 0
                classification = "CONVERSATIONAL"
            elif result.tool_filter == ToolFilter.LOCAL_BLOCKED:
                state.tools_schema = [
                    t for t in state.tools_schema
                    if t.get("function", {}).get("name") not in LOCAL_TOOLS
                ]
                state.max_rounds = min(state.max_rounds, result.max_tool_rounds)
                classification = "EXTERNAL_FETCH"
            else:
                classification = "DEFAULT"
            yield TurnEvent(event_type="stage_complete", data={"stage": "route", "classification": classification})
            return

        # Legacy: normalize IntentClassification or string
        if hasattr(result, "intent"):
            classification = result.intent
        elif isinstance(result, str):
            classification = result
        else:
            logger.warning("classify_intent returned unexpected type %s, using DEFAULT", type(result))
            classification = "DEFAULT"

        if classification == "CONVERSATIONAL":
            state.tools_schema = []
            state.max_rounds = 0
        elif classification == "EXTERNAL_FETCH":
            state.tools_schema = [
                t for t in state.tools_schema
                if t.get("function", {}).get("name") not in LOCAL_TOOLS
            ]
            state.max_rounds = min(state.max_rounds, 3)

        yield TurnEvent(event_type="stage_complete", data={"stage": "route", "classification": classification})


class CallLLMStage:
    """Loop body: call LLM, detect final answer vs tool_calls."""

    def __init__(self, llm_call: Callable):
        self._llm_call = llm_call

    async def __call__(self, state: TurnState) -> AsyncIterator[TurnEvent]:
        active = _active_tools(state)
        result = await self._llm_call(state.messages, active)

        tool_calls = result.get("tool_calls") or []
        content = result.get("content", "")

        if not tool_calls:
            # Final answer
            state.outcome = TurnOutcome(status=TurnStatus.SUCCESS, content=content)
            yield TurnEvent(event_type="llm_final", data={"content": content})
        else:
            # Append assistant message with tool_calls
            asst_msg: dict = {
                "role": "assistant",
                "content": content,
                "tool_calls": tool_calls,
            }
            # Kimi K2.5 (and other thinking models) require reasoning_content
            # to be preserved in the assistant message when tool calls are made
            if result.get("reasoning_content"):
                asst_msg["reasoning_content"] = result["reasoning_content"]
            state.messages.append(asst_msg)
            yield TurnEvent(event_type="llm_tool_calls", data={"tool_calls": tool_calls})

        yield TurnEvent(event_type="stage_complete", data={"stage": "call_llm"})


class ExecuteToolsStage:
    """Loop body: parallel asyncio.gather, breaker check, append results.

    State mutation safety: _run_one is a pure function that returns results
    without mutating TurnState. All state mutations (tool_failures, blocked_tools,
    messages) happen sequentially AFTER gather completes. This ensures correctness
    even if the executor is changed to a thread-pool in the future.
    """

    def __init__(self, tool_execute: Callable):
        self._execute = tool_execute

    async def __call__(self, state: TurnState) -> AsyncIterator[TurnEvent]:
        # Only run if last assistant message has tool_calls
        if state.outcome is not None:
            return
        last_assistant = None
        for m in reversed(state.messages):
            if m.get("role") == "assistant" and m.get("tool_calls"):
                last_assistant = m
                break
        if not last_assistant:
            return

        tool_calls = last_assistant["tool_calls"]

        async def _run_one(tc: dict) -> tuple[str, str, str | None, str]:
            """Execute one tool call — returns (call_id, result_str, error_or_None, fn_name).

            Read-only access to state (blocked_tools check). Does NOT mutate state;
            caller applies all mutations (tool_failures, blocked_tools, messages)
            sequentially after gather completes.
            """
            fn_name = tc["function"]["name"]
            call_id = tc.get("id", f"call_{fn_name}")
            raw_args = tc["function"]["arguments"]
            params = json.loads(raw_args) if isinstance(raw_args, str) else raw_args

            # Breaker check (read-only)
            if fn_name in state.blocked_tools:
                err = f"Tool {fn_name} is blocked by circuit breaker"
                return call_id, json.dumps({"error": err}), None, fn_name

            try:
                result = await self._execute(fn_name, params)
                result_str = json.dumps(result, default=str) if not isinstance(result, str) else result
                return call_id, result_str, None, fn_name
            except Exception as e:
                return call_id, json.dumps({"error": str(e)}), str(e), fn_name

        # Parallel execution — no state mutation inside tasks
        tasks = [_run_one(tc) for tc in tool_calls]
        raw_results = await asyncio.gather(*tasks)

        # Sequential state mutation — safe, deterministic order
        for call_id, result_str, error, fn_name in raw_results:
            if error is not None:
                state.tool_failures.setdefault(fn_name, []).append(error)
                if _should_break(state.tool_failures[fn_name]):
                    state.blocked_tools.add(fn_name)
            else:
                # Success: clear failure history
                state.tool_failures.pop(fn_name, None)
            state.last_skill_name = fn_name

            yield TurnEvent(
                event_type="tool_result",
                data={"call_id": call_id, "tool": fn_name, "error": error},
            )
            state.messages.append({
                "role": "tool",
                "tool_call_id": call_id,
                "content": result_str,
            })

        yield TurnEvent(event_type="stage_complete", data={"stage": "execute_tools"})


class EvaluateStage:
    """Loop body: check blocked_tools, budget, timeout → set outcome or continue."""

    async def __call__(self, state: TurnState) -> AsyncIterator[TurnEvent]:
        if state.outcome is not None:
            # Already resolved (final answer from LLM)
            yield TurnEvent(event_type="stage_complete", data={"stage": "evaluate", "action": "already_resolved"})
            return

        # Timeout check
        if _wall_clock_exceeded(state):
            state.outcome = TurnOutcome(
                status=TurnStatus.FAILURE,
                failure_reason="wall_clock_timeout",
                failed_tools=sorted(state.blocked_tools),
            )
            yield TurnEvent(event_type="stage_complete", data={"stage": "evaluate", "action": "timeout"})
            return

        # All tools blocked?
        # Guard: only trigger "all_tools_blocked" when tools_schema was non-empty.
        # If tools_schema is empty (e.g. CONVERSATIONAL intent), the loop shouldn't
        # be running at all — this is a defensive check, not the primary exit path.
        active = _active_tools(state)
        if state.tools_schema and not active:
            state.outcome = TurnOutcome(
                status=TurnStatus.FAILURE,
                failure_reason="all_tools_blocked",
                failed_tools=sorted(state.blocked_tools),
            )
            yield TurnEvent(event_type="stage_complete", data={"stage": "evaluate", "action": "all_blocked"})
            return

        yield TurnEvent(event_type="stage_complete", data={"stage": "evaluate", "action": "continue"})


class FinalAnswerStage:
    """Post-loop: ask LLM for final answer when rounds exhausted.

    Uses a local copy of messages to avoid permanently injecting a system
    message into state.messages (callers may inspect messages after the turn).
    """

    def __init__(self, final_answer_call: Callable | None = None):
        self._call = final_answer_call

    async def __call__(self, state: TurnState) -> AsyncIterator[TurnEvent]:
        if state.outcome is not None:
            # Already resolved
            yield TurnEvent(event_type="stage_complete", data={"stage": "final_answer", "action": "skip"})
            return

        # Rounds exhausted — ask for final answer using a local copy
        final_messages = [*state.messages, {
            "role": "system",
            "content": "Please provide your final answer based on the tool results above.",
        }]

        if self._call:
            content = await self._call(final_messages)
        else:
            content = "I was unable to complete the task within the allowed rounds."

        state.outcome = TurnOutcome(status=TurnStatus.EXHAUSTED, content=content)
        yield TurnEvent(event_type="stage_complete", data={"stage": "final_answer"})


# ---------------------------------------------------------------------------
# Engine: execute_turn()
# ---------------------------------------------------------------------------

async def execute_turn(
    state: TurnState,
    *,
    llm_call: Callable,
    tool_execute: Callable,
    classify_intent: Callable | None = None,
    final_answer_call: Callable | None = None,
) -> AsyncIterator[TurnEvent]:
    """Run the execution pipeline: pre_loop → loop_body (repeated) → post_loop.

    This is the ~18-line engine that never changes. All behavior lives in stages.
    """
    pipeline = ExecutionPipeline(
        pre_loop=[RouteStage(classify_intent)],
        loop_body=[
            CallLLMStage(llm_call),
            ExecuteToolsStage(tool_execute),
            EvaluateStage(),
        ],
        post_loop=[FinalAnswerStage(final_answer_call)],
    )

    # Pre-loop
    for stage in pipeline.pre_loop:
        async for event in stage(state):
            yield event

    # Loop body
    while state.outcome is None and state.round < state.max_rounds:
        # Timeout check before each round
        if _wall_clock_exceeded(state):
            state.outcome = TurnOutcome(
                status=TurnStatus.FAILURE,
                failure_reason="wall_clock_timeout",
                failed_tools=sorted(state.blocked_tools),
            )
            break

        for stage in pipeline.loop_body:
            async for event in stage(state):
                yield event
            if state.outcome is not None:
                break
        state.round += 1

    # If no rounds were executed (e.g. CONVERSATIONAL), do a single no-tools LLM call
    if state.outcome is None and state.round == 0 and state.max_rounds == 0:
        result = await llm_call(state.messages, [])
        content = result.get("content", "")
        state.outcome = TurnOutcome(status=TurnStatus.SUCCESS, content=content)

    # Post-loop
    for stage in pipeline.post_loop:
        async for event in stage(state):
            yield event

    yield TurnEvent(event_type="turn_complete", data={
        "rounds": state.round,
        "status": state.outcome.status.value if state.outcome else "unknown",
        "blocked_tools": sorted(state.blocked_tools),
    })
