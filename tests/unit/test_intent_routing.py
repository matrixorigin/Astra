"""Tests for Tier 0 dual engine + dataclasses + correction detection.

Covers: RoutingResult, ContextLoadingPlan, INTENT_PLANS, Tier0Engine, detect_correction.
"""

import pytest

from core.context.intent_routing import (
    INTENT_PLANS,
    ContextLoadingPlan,
    RoutingResult,
    Tier0Engine,
    detect_correction,
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
    @pytest.mark.parametrize("query,expected_intent", [
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
    ])
    def test_regex_matches(self, query, expected_intent):
        engine = Tier0Engine()
        result = engine._regex_classify(query)
        assert result == expected_intent, f"Expected {expected_intent} for '{query}', got {result}"

    @pytest.mark.parametrize("query", [
        "what is event sourcing?",
        "explain this error",
        "how does the memory system work?",
        "hello",
        "",
    ])
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
    @pytest.mark.parametrize("query", [
        "不对",
        "错了，不是这样",
        "你搞错了",
        "不正确",
        "wrong, that's not what I meant",
        "incorrect answer",
        "that's not right",
        "no, I said Python not Java",
        "actually, I want something different",
    ])
    def test_correction_detected(self, query):
        assert detect_correction(query) is True

    @pytest.mark.parametrize("query", [
        "what is event sourcing?",
        "run the tests",
        "记住我用vim",
        "hello",
        "explain this code",
        "",
    ])
    def test_no_correction(self, query):
        assert detect_correction(query) is False
