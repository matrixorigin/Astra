"""Tests for Tier 0 dual engine + dataclasses + correction detection + router registry.

Covers: RoutingResult, ContextLoadingPlan, INTENT_PLANS, Tier0Engine, detect_correction,
        RoutingStrategy, register_router, get_router, list_routers, TaskType, ToolFilter.
"""

import pytest

from core.context.intent_routing import (
    INTENT_PLANS,
    ContextLoadingPlan,
    RoutingDecision,
    RoutingResult,
    Tier0Engine,
    detect_correction,
    list_routers,
)


# ============================================================================
# ContextLoadingPlan + INTENT_PLANS
# ============================================================================


class TestIntentPlans:
    def test_preference_plan(self):
        p = INTENT_PLANS["preference"]
        assert p.load_tools is False
        assert p.load_history is False
        assert p.load_memory == "profile"
        assert p.estimated_tokens == 100

    def test_command_plan(self):
        p = INTENT_PLANS["command"]
        assert p.load_tools is True
        assert p.load_history is False
        assert p.load_memory is False
        assert p.estimated_tokens == 400

    def test_feedback_plan(self):
        p = INTENT_PLANS["feedback"]
        assert p.load_tools is False
        assert p.load_history == 2
        assert p.load_memory is False
        assert p.estimated_tokens == 600

    def test_question_plan(self):
        p = INTENT_PLANS["question"]
        assert p.load_tools is True
        assert p.load_history is True
        assert p.load_memory is True
        assert p.estimated_tokens == 2400

    def test_all_intents_have_plans(self):
        for intent in ("preference", "command", "feedback", "question"):
            assert intent in INTENT_PLANS


# ============================================================================
# Tier 0 Engine — Regex
# ============================================================================


class TestTier0Regex:
    @pytest.mark.parametrize(
        "query,expected_intent",
        [
            ("记住我用vim", "preference"),
            ("remember I prefer tabs", "preference"),
            ("I use pytest -n auto", "preference"),
            ("I prefer dark mode", "preference"),
            ("需要用moerr", "preference"),
            ("always use gofmt", "preference"),
            ("run the tests", "command"),
            ("execute this script", "command"),
            ("delete that file", "command"),
            ("create a new branch", "command"),
            ("list all sessions", "command"),
            ("不对", "feedback"),
            ("wrong, that's not right", "feedback"),
            ("no, I meant something else", "feedback"),
            ("actually I want Python", "feedback"),
        ],
    )
    def test_regex_matches(self, query, expected_intent):
        engine = Tier0Engine()
        result = engine._regex_classify(query)
        assert result == expected_intent, f"Expected {expected_intent} for '{query}', got {result}"

    @pytest.mark.parametrize(
        "query",
        [
            "what is event sourcing?",
            "explain this error",
            "how does the memory system work?",
            "hello",
            "",
        ],
    )
    def test_regex_no_match(self, query):
        engine = Tier0Engine()
        assert engine._regex_classify(query) is None


# ============================================================================
# Tier 0 Engine — Heuristic
# ============================================================================


class TestTier0Heuristic:
    def test_short_question_returns_none(self):
        engine = Tier0Engine()
        assert engine._heuristic_classify("what is this?", history_len=3) is None

    def test_first_turn_no_question_mark_returns_command(self):
        engine = Tier0Engine()
        assert engine._heuristic_classify("fix the bug in main.py", history_len=0) == "command"

    def test_first_turn_with_question_mark_returns_none(self):
        engine = Tier0Engine()
        assert engine._heuristic_classify("what is this?", history_len=0) is None

    def test_empty_query_returns_none(self):
        engine = Tier0Engine()
        assert engine._heuristic_classify("", history_len=0) is None

    def test_non_first_turn_long_statement_returns_none(self):
        engine = Tier0Engine()
        assert engine._heuristic_classify("fix the bug in main.py", history_len=5) is None


# ============================================================================
# Tier 0 Engine — Merge Logic
# ============================================================================


class TestTier0Merge:
    def test_both_agree_confidence_095(self):
        """First turn + 'run tests' → regex=command, heuristic=command → 0.95."""
        engine = Tier0Engine()
        result = engine.classify("run the tests", history_len=0)
        assert result.intent == "command"
        assert result.confidence == 0.95
        assert result.matched_by == "both"
        assert result.tier == 0

    def test_regex_only_confidence_080(self):
        """'记住我用vim' with history → regex=preference, heuristic=None → 0.80."""
        engine = Tier0Engine()
        result = engine.classify("记住我用vim", history_len=3)
        assert result.intent == "preference"
        assert result.confidence == 0.80
        assert result.matched_by == "regex"

    def test_heuristic_only_confidence_080(self):
        """First turn, no regex match, no question mark → heuristic=command → 0.80."""
        engine = Tier0Engine()
        result = engine.classify("fix the bug in main.py", history_len=0)
        assert result.intent == "command"
        assert result.confidence == 0.80
        assert result.matched_by == "heuristic"

    def test_neither_match_confidence_0(self):
        engine = Tier0Engine()
        result = engine.classify("what is event sourcing?", history_len=3)
        assert result.intent is None
        assert result.confidence == 0.0
        assert result.matched_by == "none"

    def test_feedback_regex_only(self):
        engine = Tier0Engine()
        result = engine.classify("不对，应该用另一个方法", history_len=5)
        assert result.intent == "feedback"
        assert result.confidence == 0.80
        assert result.matched_by == "regex"


# ============================================================================
# Correction Detection
# ============================================================================


class TestCorrectionDetection:
    @pytest.mark.parametrize(
        "query",
        [
            "不对",
            "错了，不是这样",
            "你搞错了",
            "不正确",
            "wrong, that's not what I meant",
            "incorrect answer",
            "that's not right",
            "no, I said Python not Java",
            "actually, I want something different",
        ],
    )
    def test_correction_detected(self, query):
        assert detect_correction(query) is True

    @pytest.mark.parametrize(
        "query",
        [
            "what is event sourcing?",
            "run the tests",
            "记住我用vim",
            "hello",
            "explain this code",
            "",
        ],
    )
    def test_no_correction(self, query):
        assert detect_correction(query) is False


# ============================================================================
# Router Registry
# ============================================================================


class TestRouterRegistry:
    @pytest.fixture(autouse=True)
    def _clean_registry(self):
        """Ensure custom routers registered in tests don't leak to other tests."""
        from core.context.intent_routing import _reset_registry_for_testing

        yield
        _reset_registry_for_testing()

    def test_default_registered(self):
        assert "default" in list_routers()

    def test_get_default_returns_intent_router(self):
        from core.context.intent_routing import get_router, IntentRouter
        from unittest.mock import MagicMock

        r = get_router("default", db_factory=MagicMock())
        assert isinstance(r, IntentRouter)

    def test_get_unknown_raises_key_error(self):
        from core.context.intent_routing import get_router
        from unittest.mock import MagicMock

        with pytest.raises(KeyError):
            get_router("nonexistent", db_factory=MagicMock())

    def test_duplicate_registration_raises_value_error(self):
        from core.context.intent_routing import register_router

        @register_router("dup_test")
        class First:
            def __init__(self, db_factory):
                pass

        with pytest.raises(ValueError, match="already registered"):

            @register_router("dup_test")
            class Second:
                def __init__(self, db_factory):
                    pass

    def test_register_and_instantiate_custom_router(self):
        from core.context.intent_routing import register_router, get_router, RoutingStrategy
        from unittest.mock import MagicMock

        @register_router("test_custom")
        class CustomRouter:
            def __init__(self, db_factory):
                self.db_factory = db_factory

            async def route(
                self, query, history_len=0, memory_text=None, tool_names=None, force_intent=None
            ):
                return RoutingDecision(
                    plan=INTENT_PLANS["question"],
                    routing_result=RoutingResult(
                        intent="question",
                        confidence=1.0,
                        tier=0,
                        matched_by="custom",
                    ),
                )

        assert "test_custom" in list_routers()
        r = get_router("test_custom", db_factory=MagicMock())
        assert isinstance(r, RoutingStrategy)
        assert r.db_factory is not None  # verify db_factory was passed

    def test_list_routers_sorted(self):
        from core.context.intent_routing import register_router

        @register_router("zzz_last")
        class Z:
            def __init__(self, db_factory):
                pass

        @register_router("aaa_first")
        class A:
            def __init__(self, db_factory):
                pass

        names = list_routers()
        assert names == sorted(names)

    def test_reset_preserves_default_only(self):
        from core.context.intent_routing import register_router, _reset_registry_for_testing

        @register_router("ephemeral")
        class E:
            def __init__(self, db_factory):
                pass

        assert "ephemeral" in list_routers()
        _reset_registry_for_testing()
        assert "ephemeral" not in list_routers()
        assert "default" in list_routers()


# ============================================================================
# RoutingDecision — new unified fields
# ============================================================================


class TestRoutingDecisionFields:
    """Verify RoutingDecision carries tool_filter, max_tool_rounds, task_type."""

    def test_defaults(self):
        from core.context.intent_routing import ToolFilter, TaskType, MAX_TOOL_ROUNDS

        rd = RoutingDecision(
            plan=INTENT_PLANS["question"],
            routing_result=RoutingResult(
                intent="question", confidence=0.9, tier=0, matched_by="regex"
            ),
        )
        assert rd.tool_filter == ToolFilter.NONE
        assert rd.max_tool_rounds == MAX_TOOL_ROUNDS
        assert rd.task_type == TaskType.GENERAL
        assert rd.topic_shift_score == 0.0

    def test_explicit_fields(self):
        from core.context.intent_routing import ToolFilter, TaskType

        rd = RoutingDecision(
            plan=INTENT_PLANS["command"],
            routing_result=RoutingResult(
                intent="command", confidence=0.95, tier=0, matched_by="both"
            ),
            tool_filter=ToolFilter.LOCAL_BLOCKED,
            max_tool_rounds=3,
            task_type=TaskType.DEBUGGING,
            topic_shift_score=0.5,
        )
        assert rd.tool_filter == ToolFilter.LOCAL_BLOCKED
        assert rd.max_tool_rounds == 3
        assert rd.task_type == TaskType.DEBUGGING
        assert rd.topic_shift_score == 0.5
