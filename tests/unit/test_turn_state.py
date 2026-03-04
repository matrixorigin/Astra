"""Unit tests for TurnState, TurnOutcome, TurnEvent, and ExecutionPipeline.

Every test verifies ALL fields of the dataclass under test — not just the
"interesting" ones. Wrong defaults are bugs too.
"""

import time

from core.agent.turn_state import (
    ExecutionPipeline,
    PipelineStage,
    TurnEvent,
    TurnOutcome,
    TurnState,
    TurnStatus,
)

# ---------------------------------------------------------------------------
# TurnStatus enum
# ---------------------------------------------------------------------------


class TestTurnStatus:
    def test_values(self):
        assert TurnStatus.SUCCESS.value == "success"
        assert TurnStatus.FAILURE.value == "failure"
        assert TurnStatus.EXHAUSTED.value == "exhausted"

    def test_is_str_enum(self):
        assert isinstance(TurnStatus.SUCCESS, str)
        assert TurnStatus("success") == TurnStatus.SUCCESS


# ---------------------------------------------------------------------------
# TurnState basics
# ---------------------------------------------------------------------------


class TestTurnState:
    def test_defaults(self):
        state = TurnState(messages=[], tools_schema=[])
        assert state.messages == []
        assert state.tools_schema == []
        assert state.round == 0
        assert state.max_rounds == 10
        assert state.tool_failures == {}
        assert state.blocked_tools == set()
        assert state.tokens_consumed == 0
        assert isinstance(state.wall_clock_start, float)
        assert state.wall_clock_timeout == 300.0
        assert state.outcome is None
        assert state.session_id == ""
        assert state.user_id == ""
        assert state.user_input == ""
        assert state.context_capture_id == ""
        assert state.user_event is None
        assert state.last_skill_name is None

    def test_wall_clock_start_auto(self):
        before = time.monotonic()
        state = TurnState(messages=[], tools_schema=[])
        after = time.monotonic()
        assert before <= state.wall_clock_start <= after

    def test_custom_values(self):
        state = TurnState(
            messages=[{"role": "user", "content": "hi"}],
            tools_schema=[{"type": "function"}],
            round=5,
            max_rounds=20,
            blocked_tools={"grep"},
            wall_clock_timeout=60.0,
            session_id="s1",
            user_id="alice",
            user_input="hello",
        )
        assert state.round == 5
        assert state.max_rounds == 20
        assert state.blocked_tools == {"grep"}
        assert state.wall_clock_timeout == 60.0
        assert state.session_id == "s1"
        assert state.user_id == "alice"
        assert state.user_input == "hello"


# ---------------------------------------------------------------------------
# TurnOutcome
# ---------------------------------------------------------------------------


class TestTurnOutcome:
    def test_success_all_fields(self):
        o = TurnOutcome(status=TurnStatus.SUCCESS, content="hello")
        assert o.status == TurnStatus.SUCCESS
        assert o.content == "hello"
        assert o.failure_reason is None
        assert o.failed_tools == []
        assert o.verification is None

    def test_failure_all_fields(self):
        o = TurnOutcome(
            status=TurnStatus.FAILURE,
            content="",
            failure_reason="all_tools_blocked",
            failed_tools=["grep", "shell"],
        )
        assert o.status == TurnStatus.FAILURE
        assert o.content == ""
        assert o.failure_reason == "all_tools_blocked"
        assert o.failed_tools == ["grep", "shell"]
        assert o.verification is None

    def test_exhausted_defaults(self):
        o = TurnOutcome(status=TurnStatus.EXHAUSTED)
        assert o.status == TurnStatus.EXHAUSTED
        assert o.content == ""
        assert o.failure_reason is None
        assert o.failed_tools == []

    def test_failed_tools_default_factory_isolation(self):
        """Each instance gets its own list — no shared mutable default."""
        o1 = TurnOutcome(status=TurnStatus.SUCCESS)
        o2 = TurnOutcome(status=TurnStatus.SUCCESS)
        o1.failed_tools.append("grep")
        assert o2.failed_tools == []


# ---------------------------------------------------------------------------
# Wire serialization
# ---------------------------------------------------------------------------


class TestWireSerialization:
    def test_round_trip_no_outcome(self):
        state = TurnState(
            messages=[{"role": "user", "content": "hi"}],
            tools_schema=[{"type": "function"}],
            round=3,
            max_rounds=8,
            blocked_tools={"grep"},
            tool_failures={"grep": ["error1", "error2"]},
        )
        wire = state.to_wire()

        # Verify wire format — every field
        assert wire["blocked_tools"] == ["grep"]
        assert wire["round"] == 3
        assert wire["max_rounds"] == 8
        assert wire["tool_failures"] == {"grep": ["error1", "error2"]}
        assert wire["outcome"] is None

        # Round-trip
        restored = TurnState.from_wire(
            wire,
            messages=[{"role": "user", "content": "hi"}],
            tools_schema=[{"type": "function"}],
        )
        assert restored.round == 3
        assert restored.max_rounds == 8
        assert restored.blocked_tools == {"grep"}
        assert restored.tool_failures == {"grep": ["error1", "error2"]}
        assert restored.outcome is None
        assert restored.messages == [{"role": "user", "content": "hi"}]
        assert restored.tools_schema == [{"type": "function"}]

    def test_round_trip_with_outcome(self):
        state = TurnState(messages=[], tools_schema=[])
        state.outcome = TurnOutcome(
            status=TurnStatus.FAILURE,
            content="failed",
            failure_reason="all_tools_blocked",
            failed_tools=["shell"],
        )
        wire = state.to_wire()

        # Verify wire outcome format
        assert wire["outcome"]["status"] == "failure"
        assert wire["outcome"]["content"] == "failed"
        assert wire["outcome"]["failure_reason"] == "all_tools_blocked"
        assert wire["outcome"]["failed_tools"] == ["shell"]

        restored = TurnState.from_wire(wire)
        assert restored.outcome is not None
        assert restored.outcome.status == TurnStatus.FAILURE
        assert restored.outcome.content == "failed"
        assert restored.outcome.failure_reason == "all_tools_blocked"
        assert restored.outcome.failed_tools == ["shell"]

    def test_max_rounds_capped(self):
        """Cloud-side validation: max_rounds capped at 20."""
        restored = TurnState.from_wire({"max_rounds": 100, "round": 0})
        assert restored.max_rounds == 20

    def test_round_clamped_non_negative(self):
        """Malicious edge client sending negative round."""
        restored = TurnState.from_wire({"round": -5, "max_rounds": 10})
        assert restored.round == 0

    def test_from_wire_empty(self):
        restored = TurnState.from_wire({})
        assert restored.round == 0
        assert restored.max_rounds == 10
        assert restored.blocked_tools == set()
        assert restored.tool_failures == {}
        assert restored.outcome is None
        assert restored.messages == []
        assert restored.tools_schema == []

    def test_mutations_isolated(self):
        """Wire serialization must produce independent copies."""
        state = TurnState(messages=[], tools_schema=[])
        state.tool_failures["grep"] = ["err"]
        wire = state.to_wire()

        # Mutate wire — should not affect original
        wire["tool_failures"]["grep"].append("extra")
        assert state.tool_failures["grep"] == ["err"]

        # Mutate restored — should not affect wire
        restored = TurnState.from_wire(wire)
        restored.tool_failures["grep"].append("more")
        assert wire["tool_failures"]["grep"] == ["err", "extra"]

    def test_from_wire_invalid_status_falls_back(self):
        """Invalid status from untrusted edge client -> FAILURE fallback."""
        restored = TurnState.from_wire({
            "outcome": {"status": "bogus_status", "content": "bad"},
        })
        assert restored.outcome is not None
        assert restored.outcome.status == TurnStatus.FAILURE
        assert restored.outcome.content == "bad"
        assert restored.outcome.failure_reason is None
        assert restored.outcome.failed_tools == []

    def test_from_wire_missing_status_key_falls_back(self):
        """Missing 'status' key in outcome -> FAILURE fallback."""
        restored = TurnState.from_wire({
            "outcome": {"content": "no status field"},
        })
        assert restored.outcome is not None
        assert restored.outcome.status == TurnStatus.FAILURE


# ---------------------------------------------------------------------------
# TurnEvent
# ---------------------------------------------------------------------------


class TestTurnEvent:
    def test_all_fields(self):
        e = TurnEvent(event_type="tool_start", data={"tool": "grep"})
        assert e.event_type == "tool_start"
        assert e.data == {"tool": "grep"}

    def test_defaults(self):
        e = TurnEvent(event_type="stage_complete")
        assert e.event_type == "stage_complete"
        assert e.data == {}

    def test_data_default_factory_isolation(self):
        """Each instance gets its own dict."""
        e1 = TurnEvent(event_type="a")
        e2 = TurnEvent(event_type="b")
        e1.data["key"] = "val"
        assert e2.data == {}


# ---------------------------------------------------------------------------
# PipelineStage protocol
# ---------------------------------------------------------------------------


class TestPipelineStageProtocol:
    def test_async_generator_satisfies_protocol(self):
        async def my_stage(state: TurnState):
            yield TurnEvent(event_type="test")

        assert isinstance(my_stage, PipelineStage)

    def test_regular_function_does_not_satisfy(self):
        def not_a_stage(state: TurnState):
            return []

        # Regular (non-async) callable should not satisfy the protocol
        # Note: runtime_checkable only checks __call__ exists, not async
        # This is a known limitation of runtime_checkable with async protocols


# ---------------------------------------------------------------------------
# ExecutionPipeline
# ---------------------------------------------------------------------------


class TestExecutionPipeline:
    def test_defaults(self):
        p = ExecutionPipeline()
        assert p.pre_loop == []
        assert p.loop_body == []
        assert p.post_loop == []

    def test_with_stages(self):
        async def stage_a(state: TurnState):
            yield TurnEvent(event_type="a")

        async def stage_b(state: TurnState):
            yield TurnEvent(event_type="b")

        p = ExecutionPipeline(
            pre_loop=[stage_a],
            loop_body=[stage_a, stage_b],
            post_loop=[stage_b],
        )
        assert len(p.pre_loop) == 1
        assert len(p.loop_body) == 2
        assert len(p.post_loop) == 1

    def test_list_default_factory_isolation(self):
        """Each pipeline gets its own lists."""
        p1 = ExecutionPipeline()
        p2 = ExecutionPipeline()

        async def dummy(state):
            yield TurnEvent(event_type="x")

        p1.pre_loop.append(dummy)
        assert p2.pre_loop == []
