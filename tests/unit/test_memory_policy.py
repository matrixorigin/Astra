"""Tests for memory policy as first-class runtime capability."""

from core.memory.policy import (
    MemoryContextMode,
    MemoryPolicy,
    build_memory_guard_payload,
    evaluate_memory_tool_call,
)


class TestMemoryPolicyToolHints:
    def test_profile_question_prefers_memory_profile(self):
        policy = MemoryPolicy().decide("你记得我什么偏好？")
        assert policy.tool_hint.tool_name == "memory_profile"
        assert policy.context_plan.mode == MemoryContextMode.PROFILE_ONLY

    def test_browse_question_prefers_memory_search(self):
        policy = MemoryPolicy().decide("what do you know about matrixone?")
        assert policy.tool_hint.tool_name == "memory_search"
        assert policy.context_plan.mode == MemoryContextMode.SEARCH

    def test_recall_question_prefers_memory_retrieve(self):
        policy = MemoryPolicy().decide("what did I say about pytest flags?")
        assert policy.tool_hint.tool_name == "memory_retrieve"
        assert policy.context_plan.mode == MemoryContextMode.RETRIEVE

    def test_store_preference_prefers_profile_store(self):
        policy = MemoryPolicy().decide("remember that I use vim by default")
        assert policy.tool_hint.tool_name == "memory_store"
        assert policy.tool_hint.memory_type == "profile"
        assert policy.context_plan.mode == MemoryContextMode.PROFILE_ONLY

    def test_correction_prefers_memory_correct(self):
        policy = MemoryPolicy().decide("更正一下，不是 black，是 ruff")
        assert policy.tool_hint.tool_name == "memory_correct"
        assert policy.context_plan.mode == MemoryContextMode.SEARCH

    def test_purge_prefers_memory_purge(self):
        policy = MemoryPolicy().decide("forget that compiler preference")
        assert policy.tool_hint.tool_name == "memory_purge"
        assert policy.context_plan.mode == MemoryContextMode.SEARCH


class TestMemoryPolicyRoutingFallback:
    def test_routing_false_can_be_overridden_by_explicit_memory_query(self):
        policy = MemoryPolicy().decide("what do you remember about our api design?", load_memory=False)
        assert policy.context_plan.mode == MemoryContextMode.RETRIEVE

    def test_command_mode_still_skips_memory_when_no_memory_signal(self):
        policy = MemoryPolicy().decide("run pytest -n auto", load_memory=False)
        assert policy.context_plan.mode == MemoryContextMode.NONE


class TestMemoryExecutionGuard:
    def test_store_blocks_non_memory_tool(self):
        policy = MemoryPolicy().decide("remember that I use vim by default")
        decision = evaluate_memory_tool_call(
            actual_tool="bash",
            tool_hint=policy.tool_hint,
            available_tools={"memory_store", "bash"},
        )
        payload = build_memory_guard_payload(decision)

        assert decision.allow is False
        assert decision.outcome == "non_memory_tool"
        assert payload["expected_tool"] == "memory_store"
        assert payload["suggested_memory_type"] == "profile"

    def test_correct_allows_search_as_precursor(self):
        policy = MemoryPolicy().decide("更正一下，不是 black，是 ruff")
        decision = evaluate_memory_tool_call(
            actual_tool="memory_search",
            tool_hint=policy.tool_hint,
            available_tools={"memory_search", "memory_correct"},
        )

        assert decision.allow is True
        assert decision.outcome == "compatible_memory_tool"

    def test_unavailable_preferred_tool_does_not_block(self):
        policy = MemoryPolicy().decide("what do you remember about pytest flags?")
        decision = evaluate_memory_tool_call(
            actual_tool="read_file",
            tool_hint=policy.tool_hint,
            available_tools={"read_file"},
        )

        assert decision.allow is True
        assert decision.outcome == "preferred_unavailable"
