"""Test suite for the execution pipeline.

Tests the target API: execute_turn(state) -> AsyncIterator[TurnEvent].
All tests use mock LLM + mock tool executor.

Every test verifies:
- Final TurnState fields (outcome, round, blocked_tools, tool_failures, messages)
- Emitted TurnEvent stream (types, order, data fields)
"""

import asyncio
import json
import time
from collections.abc import AsyncIterator
from unittest.mock import AsyncMock

import pytest

from core.agent.pipeline_stages import execute_turn
from core.agent.turn_state import (
    TurnEvent,
    TurnState,
    TurnStatus,
)

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def _tool_schema(name: str) -> dict:
    return {
        "type": "function",
        "function": {
            "name": name,
            "description": f"Tool {name}",
            "parameters": {"type": "object", "properties": {}},
        },
    }


def _tool_call(name: str, args: dict | None = None, call_id: str | None = None) -> dict:
    return {
        "id": call_id or f"call_{name}",
        "function": {
            "name": name,
            "arguments": json.dumps(args or {}),
        },
    }


async def _collect_events(ait: AsyncIterator[TurnEvent]) -> list[TurnEvent]:
    events = []
    async for e in ait:
        events.append(e)
    return events


def _make_state(**overrides) -> TurnState:
    defaults = {
        "messages": [{"role": "user", "content": "test"}],
        "tools_schema": [_tool_schema("grep"), _tool_schema("shell")],
        "max_rounds": 10,
        "user_input": "test",
        "session_id": "sess-1",
        "user_id": "alice",
    }
    defaults.update(overrides)
    return TurnState(**defaults)


# ---------------------------------------------------------------------------
# Exit path: all tools blocked
# ---------------------------------------------------------------------------


class TestAllToolsBlocked:
    """2 tools, both fail 3x, verify outcome.status == 'failure'."""

    @pytest.mark.asyncio
    async def test_all_tools_blocked(self):
        """When every tool hits the breaker, turn exits with failure."""
        call_count = 0

        async def mock_llm_call(messages, tools, **kw):
            nonlocal call_count
            call_count += 1
            name = "grep" if call_count % 2 == 1 else "shell"
            return {
                "content": "",
                "tool_calls": [_tool_call(name, call_id=f"call_{call_count}")],
            }

        async def mock_execute(name, params, **kw):
            raise RuntimeError(f"{name} failed: connection refused")

        state = _make_state(max_rounds=20)
        events = await _collect_events(
            execute_turn(state, llm_call=mock_llm_call, tool_execute=mock_execute)
        )

        # Verify outcome — every field
        assert state.outcome is not None
        assert state.outcome.status == TurnStatus.FAILURE
        assert state.outcome.content == ""
        assert state.outcome.failure_reason == "all_tools_blocked"
        assert sorted(state.outcome.failed_tools) == ["grep", "shell"]
        assert state.outcome.verification is None

        # Verify breaker state
        assert state.blocked_tools == {"grep", "shell"}
        assert "grep" in state.tool_failures
        assert "shell" in state.tool_failures

        # Verify event stream ends with turn_complete
        assert events[-1].event_type == "turn_complete"
        assert events[-1].data["status"] == "failure"
        assert sorted(events[-1].data["blocked_tools"]) == ["grep", "shell"]

        # Verify tool_result events were emitted for each failure
        tool_results = [e for e in events if e.event_type == "tool_result"]
        assert len(tool_results) > 0
        assert all(e.data.get("error") is not None for e in tool_results)

    @pytest.mark.asyncio
    async def test_all_blocked_does_not_trigger_on_empty_schema(self):
        """EvaluateStage guard: empty tools_schema should NOT trigger all_tools_blocked."""

        async def mock_llm_call(messages, tools, **kw):
            return {"content": "answer", "tool_calls": []}

        state = _make_state(tools_schema=[], max_rounds=1)
        await _collect_events(execute_turn(state, llm_call=mock_llm_call, tool_execute=AsyncMock()))

        assert state.outcome is not None
        assert state.outcome.status == TurnStatus.SUCCESS
        assert state.outcome.failure_reason is None


# ---------------------------------------------------------------------------
# Exit path: similar error circuit break
# ---------------------------------------------------------------------------


class TestSimilarErrorCircuitBreak:
    """Same tool fails 2x with >60% word overlap -- blocked after 2nd failure."""

    @pytest.mark.asyncio
    async def test_similar_errors_break_tool(self):
        call_count = 0

        async def mock_llm_call(messages, tools, **kw):
            nonlocal call_count
            call_count += 1
            if call_count <= 2:
                return {
                    "content": "",
                    "tool_calls": [_tool_call("grep", call_id=f"call_{call_count}")],
                }
            return {"content": "done", "tool_calls": []}

        async def mock_execute(name, params, **kw):
            raise RuntimeError("connection refused to remote host server")

        state = _make_state()
        await _collect_events(
            execute_turn(state, llm_call=mock_llm_call, tool_execute=mock_execute)
        )

        # Verify grep was blocked after 2 similar errors
        assert "grep" in state.blocked_tools
        assert len(state.tool_failures["grep"]) == 2

        # Verify final outcome is success (LLM returned "done" after grep blocked)
        assert state.outcome is not None
        assert state.outcome.status == TurnStatus.SUCCESS
        assert state.outcome.content == "done"

    @pytest.mark.asyncio
    async def test_dissimilar_errors_do_not_break_at_two(self):
        """Two failures with different error messages should NOT trip the breaker."""
        call_count = 0
        errors = ["connection refused", "permission denied access forbidden"]

        async def mock_llm_call(messages, tools, **kw):
            nonlocal call_count
            call_count += 1
            if call_count <= 2:
                return {
                    "content": "",
                    "tool_calls": [_tool_call("grep", call_id=f"call_{call_count}")],
                }
            return {"content": "done", "tool_calls": []}

        async def mock_execute(name, params, **kw):
            raise RuntimeError(errors[call_count - 1])

        state = _make_state()
        await _collect_events(
            execute_turn(state, llm_call=mock_llm_call, tool_execute=mock_execute)
        )

        # Dissimilar errors: grep should NOT be blocked after only 2
        assert "grep" not in state.blocked_tools
        assert len(state.tool_failures.get("grep", [])) == 2


# ---------------------------------------------------------------------------
# Exit path: max_rounds exhausted
# ---------------------------------------------------------------------------


class TestMaxRoundsExhausted:
    @pytest.mark.asyncio
    async def test_exhausted_rounds(self):
        round_count = 0

        async def mock_llm_call(messages, tools, **kw):
            nonlocal round_count
            round_count += 1
            return {
                "content": "",
                "tool_calls": [_tool_call("grep", call_id=f"call_{round_count}")],
            }

        async def mock_execute(name, params, **kw):
            return {"output": "ok"}

        async def mock_final_answer(messages, **kw):
            return "Final answer after exhaustion"

        state = _make_state(max_rounds=3)
        events = await _collect_events(
            execute_turn(
                state,
                llm_call=mock_llm_call,
                tool_execute=mock_execute,
                final_answer_call=mock_final_answer,
            )
        )

        # Verify outcome — every field
        assert state.outcome is not None
        assert state.outcome.status == TurnStatus.EXHAUSTED
        assert state.outcome.content == "Final answer after exhaustion"
        assert state.outcome.failure_reason is None
        assert state.outcome.failed_tools == []
        assert state.round == 3

        # Verify messages were NOT mutated by FinalAnswerStage
        system_msgs = [
            m
            for m in state.messages
            if m.get("role") == "system" and "final answer" in m.get("content", "").lower()
        ]
        assert len(system_msgs) == 0, "FinalAnswerStage should not mutate state.messages"

        # Verify turn_complete event
        assert events[-1].event_type == "turn_complete"
        assert events[-1].data["rounds"] == 3
        assert events[-1].data["status"] == "exhausted"

    @pytest.mark.asyncio
    async def test_exhausted_without_final_answer_call(self):
        """When no final_answer_call is provided, use default message."""

        async def mock_llm_call(messages, tools, **kw):
            return {"content": "", "tool_calls": [_tool_call("grep")]}

        async def mock_execute(name, params, **kw):
            return {"output": "ok"}

        state = _make_state(max_rounds=1)
        await _collect_events(
            execute_turn(state, llm_call=mock_llm_call, tool_execute=mock_execute)
        )

        assert state.outcome is not None
        assert state.outcome.status == TurnStatus.EXHAUSTED
        assert "unable to complete" in state.outcome.content.lower()


# ---------------------------------------------------------------------------
# Exit path: wall_clock timeout
# ---------------------------------------------------------------------------


class TestWallClockTimeout:
    @pytest.mark.asyncio
    async def test_timeout_exit(self):
        """Timeout is checked at the start of each round, not during tool execution.

        Flow: round 0 starts (not timed out yet) → tool sleeps 0.5s → round 0 ends →
        round 1 starts → timeout check fires (0.5s > 0.1s) → exit.
        """

        async def mock_llm_call(messages, tools, **kw):
            return {"content": "", "tool_calls": [_tool_call("grep")]}

        async def mock_execute(name, params, **kw):
            await asyncio.sleep(0.5)
            return {"output": "ok"}

        state = _make_state(wall_clock_timeout=0.1)
        state.wall_clock_start = time.monotonic()

        events = await _collect_events(
            execute_turn(state, llm_call=mock_llm_call, tool_execute=mock_execute)
        )

        # Verify outcome — every field
        assert state.outcome is not None
        assert state.outcome.status == TurnStatus.FAILURE
        assert state.outcome.failure_reason == "wall_clock_timeout"
        assert state.outcome.failed_tools == []  # no tools were blocked, just timed out
        assert state.outcome.content == ""

        # At least 1 round completed before timeout
        assert state.round >= 1

        # Verify turn_complete
        assert events[-1].event_type == "turn_complete"
        assert events[-1].data["status"] == "failure"


# ---------------------------------------------------------------------------
# Happy path: LLM returns final answer on round 1
# ---------------------------------------------------------------------------


class TestHappyPathFinalAnswer:
    @pytest.mark.asyncio
    async def test_immediate_answer(self):
        async def mock_llm_call(messages, tools, **kw):
            return {"content": "The answer is 42", "tool_calls": []}

        state = _make_state()
        events = await _collect_events(
            execute_turn(state, llm_call=mock_llm_call, tool_execute=AsyncMock())
        )

        # Verify outcome — every field
        assert state.outcome is not None
        assert state.outcome.status == TurnStatus.SUCCESS
        assert state.outcome.content == "The answer is 42"
        assert state.outcome.failure_reason is None
        assert state.outcome.failed_tools == []
        assert state.outcome.verification is None

        # Verify state
        assert state.round == 1  # one round executed (LLM called, no tools)
        assert state.blocked_tools == set()
        assert state.tool_failures == {}

        # Verify event stream order
        event_types = [e.event_type for e in events]
        assert event_types[0] == "stage_complete"  # route stage
        assert "llm_final" in event_types
        assert event_types[-1] == "turn_complete"

        # Verify turn_complete data
        assert events[-1].data["rounds"] == 1
        assert events[-1].data["status"] == "success"
        assert events[-1].data["blocked_tools"] == []


# ---------------------------------------------------------------------------
# Happy path: tool call → result → final answer
# ---------------------------------------------------------------------------


class TestToolCallThenAnswer:
    @pytest.mark.asyncio
    async def test_one_tool_call_then_answer(self):
        """LLM calls tool once, gets result, then gives final answer."""
        call_count = 0

        async def mock_llm_call(messages, tools, **kw):
            nonlocal call_count
            call_count += 1
            if call_count == 1:
                return {
                    "content": "",
                    "tool_calls": [_tool_call("grep", {"pattern": "foo"}, "call_1")],
                }
            return {"content": "Found foo in bar.py", "tool_calls": []}

        async def mock_execute(name, params, **kw):
            return {"matches": ["bar.py:10: foo"]}

        state = _make_state()
        events = await _collect_events(
            execute_turn(state, llm_call=mock_llm_call, tool_execute=mock_execute)
        )

        # Verify outcome
        assert state.outcome.status == TurnStatus.SUCCESS
        assert state.outcome.content == "Found foo in bar.py"
        assert state.round == 2  # round 1: tool call, round 2: final answer

        # Verify messages chain: user → assistant+tool_calls → tool → assistant(final)
        roles = [m["role"] for m in state.messages]
        assert roles[0] == "user"
        assert roles[1] == "assistant"
        assert state.messages[1].get("tool_calls") is not None
        assert roles[2] == "tool"
        assert state.messages[2]["tool_call_id"] == "call_1"
        # Tool result should contain the grep output
        tool_content = json.loads(state.messages[2]["content"])
        assert tool_content == {"matches": ["bar.py:10: foo"]}

        # Verify event stream includes tool_result
        tool_results = [e for e in events if e.event_type == "tool_result"]
        assert len(tool_results) == 1
        assert tool_results[0].data["tool"] == "grep"
        assert tool_results[0].data["error"] is None
        assert tool_results[0].data["call_id"] == "call_1"

        # Verify no tools were blocked
        assert state.blocked_tools == set()
        assert state.tool_failures == {}


# ---------------------------------------------------------------------------
# Happy path: tool succeeds, clears failure history
# ---------------------------------------------------------------------------


class TestToolSuccessClearsFailures:
    @pytest.mark.asyncio
    async def test_success_clears_history(self):
        call_count = 0

        async def mock_llm_call(messages, tools, **kw):
            nonlocal call_count
            call_count += 1
            if call_count <= 2:
                return {
                    "content": "",
                    "tool_calls": [_tool_call("grep", call_id=f"call_{call_count}")],
                }
            return {"content": "done", "tool_calls": []}

        async def mock_execute(name, params, **kw):
            if call_count == 1:
                raise RuntimeError("temporary failure")
            return {"output": "success"}

        state = _make_state()
        await _collect_events(
            execute_turn(state, llm_call=mock_llm_call, tool_execute=mock_execute)
        )

        # After success, failure history should be fully cleared (pop, not empty list)
        assert "grep" not in state.tool_failures
        assert "grep" not in state.blocked_tools
        assert state.outcome.status == TurnStatus.SUCCESS


# ---------------------------------------------------------------------------
# Routing: CONVERSATIONAL blocks all tools
# ---------------------------------------------------------------------------


class TestRoutingConversational:
    @pytest.mark.asyncio
    async def test_conversational_no_tools(self):
        async def mock_classify(query):
            return "CONVERSATIONAL"

        llm_called_with_tools = None

        async def mock_llm_call(messages, tools, **kw):
            nonlocal llm_called_with_tools
            llm_called_with_tools = tools
            return {"content": "Hello!", "tool_calls": []}

        state = _make_state()
        events = await _collect_events(
            execute_turn(
                state,
                llm_call=mock_llm_call,
                tool_execute=AsyncMock(),
                classify_intent=mock_classify,
            )
        )

        # Verify state mutations from RouteStage
        assert state.tools_schema == []
        assert state.max_rounds == 0

        # Verify outcome
        assert state.outcome is not None
        assert state.outcome.status == TurnStatus.SUCCESS
        assert state.outcome.content == "Hello!"

        # Verify LLM was called with empty tools
        assert llm_called_with_tools == []

        # Verify round count: 0 rounds in loop, but CONVERSATIONAL special-case ran
        assert state.round == 0

        # Verify route stage_complete event
        route_events = [
            e for e in events if e.event_type == "stage_complete" and e.data.get("stage") == "route"
        ]
        assert len(route_events) == 1
        assert route_events[0].data["classification"] == "CONVERSATIONAL"


# ---------------------------------------------------------------------------
# Routing: EXTERNAL_FETCH blocks local tools
# ---------------------------------------------------------------------------


class TestRoutingExternalFetch:
    @pytest.mark.asyncio
    async def test_external_fetch_filters_tools(self):
        async def mock_classify(query):
            return "EXTERNAL_FETCH"

        seen_tools = []

        async def mock_llm_call(messages, tools, **kw):
            seen_tools.extend(tools or [])
            return {"content": "fetched", "tool_calls": []}

        state = _make_state(
            tools_schema=[
                _tool_schema("grep"),
                _tool_schema("shell"),
                _tool_schema("web_search"),
            ],
        )
        events = await _collect_events(
            execute_turn(
                state,
                llm_call=mock_llm_call,
                tool_execute=AsyncMock(),
                classify_intent=mock_classify,
            )
        )

        # Verify local tools were filtered from schema
        remaining_names = [t["function"]["name"] for t in state.tools_schema]
        assert "grep" not in remaining_names
        assert "shell" not in remaining_names
        assert "web_search" in remaining_names

        # Verify max_rounds capped
        assert state.max_rounds <= 3

        # Verify LLM only saw non-local tools
        tool_names = [t["function"]["name"] for t in seen_tools]
        assert "grep" not in tool_names
        assert "shell" not in tool_names

        # Verify route event
        route_events = [
            e for e in events if e.event_type == "stage_complete" and e.data.get("stage") == "route"
        ]
        assert len(route_events) == 1
        assert route_events[0].data["classification"] == "EXTERNAL_FETCH"


# ---------------------------------------------------------------------------
# Routing: RoutingDecision from unified router
# ---------------------------------------------------------------------------


class TestRoutingWithRoutingDecision:
    """RouteStage must handle RoutingDecision objects from the unified router."""

    @pytest.mark.asyncio
    async def test_routing_decision_default(self):
        """RoutingDecision with NONE tool_filter → no changes."""
        from core.context.intent_routing import (
            INTENT_PLANS,
            RoutingDecision,
            RoutingResult,
            ToolFilter,
        )

        decision = RoutingDecision(
            plan=INTENT_PLANS["question"],
            routing_result=RoutingResult(
                intent="question", confidence=0.9, tier=0, matched_by="regex"
            ),
            tool_filter=ToolFilter.NONE,
        )

        async def mock_llm_call(messages, tools, **kw):
            return {"content": "Answer", "tool_calls": []}

        state = _make_state()
        original_tools = list(state.tools_schema)
        await _collect_events(
            execute_turn(
                state,
                llm_call=mock_llm_call,
                tool_execute=AsyncMock(),
                classify_intent=lambda q: decision,
            )
        )
        assert state.outcome is not None
        assert state.outcome.status == TurnStatus.SUCCESS

    @pytest.mark.asyncio
    async def test_routing_decision_all_blocked(self):
        """RoutingDecision with ALL_BLOCKED → tools cleared, max_rounds=0."""
        from core.context.intent_routing import (
            INTENT_PLANS,
            RoutingDecision,
            RoutingResult,
            ToolFilter,
        )

        decision = RoutingDecision(
            plan=INTENT_PLANS["preference"],
            routing_result=RoutingResult(
                intent="preference", confidence=0.9, tier=0, matched_by="regex"
            ),
            tool_filter=ToolFilter.ALL_BLOCKED,
            max_tool_rounds=0,
        )

        async def mock_llm_call(messages, tools, **kw):
            return {"content": "Hi there!", "tool_calls": []}

        state = _make_state(user_input="hello")
        await _collect_events(
            execute_turn(
                state,
                llm_call=mock_llm_call,
                tool_execute=AsyncMock(),
                classify_intent=lambda q: decision,
            )
        )
        assert state.tools_schema == []
        assert state.max_rounds == 0
        assert state.outcome.status == TurnStatus.SUCCESS
        assert state.outcome.content == "Hi there!"


# ---------------------------------------------------------------------------
# Parallel execution
# ---------------------------------------------------------------------------


class TestParallelExecution:
    """3 tool_calls in one round -- all execute concurrently."""

    @pytest.mark.asyncio
    async def test_parallel_tool_calls(self):
        execution_times = {}

        async def mock_llm_call(messages, tools, **kw):
            if not any(m.get("role") == "tool" for m in messages):
                return {
                    "content": "",
                    "tool_calls": [
                        _tool_call("tool_a", call_id="a"),
                        _tool_call("tool_b", call_id="b"),
                        _tool_call("tool_c", call_id="c"),
                    ],
                }
            return {"content": "all done", "tool_calls": []}

        async def mock_execute(name, params, **kw):
            start = time.monotonic()
            await asyncio.sleep(0.1)
            execution_times[name] = time.monotonic() - start
            return {"output": f"{name} result"}

        state = _make_state(
            tools_schema=[_tool_schema("tool_a"), _tool_schema("tool_b"), _tool_schema("tool_c")],
        )
        t0 = time.monotonic()
        events = await _collect_events(
            execute_turn(state, llm_call=mock_llm_call, tool_execute=mock_execute)
        )
        total = time.monotonic() - t0

        # Verify all 3 tools executed
        assert len(execution_times) == 3
        # If parallel, total should be ~0.1s not ~0.3s.
        # Use generous bound (0.5s) to avoid flakiness on loaded CI machines.
        assert total < 0.5, f"Expected parallel execution, took {total:.2f}s"

        # Verify outcome
        assert state.outcome is not None
        assert state.outcome.status == TurnStatus.SUCCESS
        assert state.outcome.content == "all done"

        # Verify all 3 tool results in messages
        tool_msgs = [m for m in state.messages if m.get("role") == "tool"]
        assert len(tool_msgs) == 3
        tool_call_ids = {m["tool_call_id"] for m in tool_msgs}
        assert tool_call_ids == {"a", "b", "c"}

        # Verify 3 tool_result events emitted
        tool_result_events = [e for e in events if e.event_type == "tool_result"]
        assert len(tool_result_events) == 3
        result_tools = {e.data["tool"] for e in tool_result_events}
        assert result_tools == {"tool_a", "tool_b", "tool_c"}
        assert all(e.data["error"] is None for e in tool_result_events)


# ---------------------------------------------------------------------------
# Observability: event stream structure
# ---------------------------------------------------------------------------


class TestObservability:
    @pytest.mark.asyncio
    async def test_event_stream_structure_happy_path(self):
        """Verify complete event stream order for a simple happy path."""

        async def mock_llm_call(messages, tools, **kw):
            return {"content": "answer", "tool_calls": []}

        state = _make_state()
        events = await _collect_events(
            execute_turn(state, llm_call=mock_llm_call, tool_execute=AsyncMock())
        )

        event_types = [e.event_type for e in events]

        # Expected order: route(stage_complete) → call_llm(llm_final, stage_complete)
        #   → evaluate(stage_complete) → final_answer(stage_complete) → turn_complete
        assert event_types[0] == "stage_complete"  # route
        assert events[0].data["stage"] == "route"
        assert "llm_final" in event_types
        assert event_types[-1] == "turn_complete"

        # turn_complete must be the LAST event
        assert events[-1].event_type == "turn_complete"
        assert events[-1].data["rounds"] == 1
        assert events[-1].data["status"] == "success"
        assert events[-1].data["blocked_tools"] == []

    @pytest.mark.asyncio
    async def test_event_stream_with_tool_call(self):
        """Verify event stream includes tool_call and tool_result events."""
        call_count = 0

        async def mock_llm_call(messages, tools, **kw):
            nonlocal call_count
            call_count += 1
            if call_count == 1:
                return {"content": "", "tool_calls": [_tool_call("grep", call_id="c1")]}
            return {"content": "done", "tool_calls": []}

        async def mock_execute(name, params, **kw):
            return {"output": "found"}

        state = _make_state()
        events = await _collect_events(
            execute_turn(state, llm_call=mock_llm_call, tool_execute=mock_execute)
        )

        event_types = [e.event_type for e in events]
        assert "llm_tool_calls" in event_types
        assert "tool_result" in event_types
        assert "llm_final" in event_types

        # Verify tool_result data fields
        tr = next(e for e in events if e.event_type == "tool_result")
        assert tr.data["call_id"] == "c1"
        assert tr.data["tool"] == "grep"
        assert tr.data["error"] is None


# ---------------------------------------------------------------------------
# Edge case: blocked tool in parallel batch
# ---------------------------------------------------------------------------


class TestBlockedToolInParallelBatch:
    @pytest.mark.asyncio
    async def test_blocked_tool_returns_error_in_batch(self):
        """If a tool is already blocked, it returns an error without executing."""
        call_count = 0

        async def mock_llm_call(messages, tools, **kw):
            nonlocal call_count
            call_count += 1
            if call_count == 1:
                # Request both tools — grep is pre-blocked
                return {
                    "content": "",
                    "tool_calls": [
                        _tool_call("grep", call_id="c1"),
                        _tool_call("shell", call_id="c2"),
                    ],
                }
            return {"content": "done", "tool_calls": []}

        executed_tools = []

        async def mock_execute(name, params, **kw):
            executed_tools.append(name)
            return {"output": "ok"}

        state = _make_state()
        state.blocked_tools.add("grep")  # Pre-block grep

        await _collect_events(
            execute_turn(state, llm_call=mock_llm_call, tool_execute=mock_execute)
        )

        # grep should NOT have been executed
        assert "grep" not in executed_tools
        # shell should have been executed
        assert "shell" in executed_tools

        # Verify grep's tool message contains error
        grep_msg = next(
            m for m in state.messages if m.get("role") == "tool" and m.get("tool_call_id") == "c1"
        )
        assert "blocked" in grep_msg["content"].lower()
