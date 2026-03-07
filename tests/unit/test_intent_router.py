"""Tests for unified intent routing: KeywordRegistry, Tier0Engine tool_filter/task_type.

Replaces old test_intent_router.py (classify_intent) — now tests the unified
Tier0Engine.classify_tool_filter() and classify_task_type() methods.
"""

import pytest

from core.context.intent_routing import (
    KeywordRegistry,
    RegistryMatch,
    TaskType,
    Tier0Engine,
    ToolFilter,
)


# ============================================================================
# KeywordRegistry
# ============================================================================

class TestKeywordRegistry:
    def test_match_returns_best_label(self):
        reg = KeywordRegistry("test", {"a": ["hello", "hi"], "b": ["bye"]})
        result = reg.match("hello there")
        assert result.label == "a"
        assert result.score > 0

    def test_no_match_returns_none(self):
        reg = KeywordRegistry("test", {"a": ["hello"]})
        result = reg.match("goodbye world")
        assert result.label is None
        assert result.score == 0.0

    def test_empty_query(self):
        reg = KeywordRegistry("test", {"a": ["hello"]})
        result = reg.match("")
        assert result.label is None

    def test_negative_keywords_suppress(self):
        reg = KeywordRegistry(
            "test",
            keywords={"fetch": ["search"]},
            negative_keywords={"fetch": ["code", "file"]},
        )
        assert reg.match("search online").label == "fetch"
        assert reg.match("search the code").label is None

    def test_cjk_keywords(self):
        reg = KeywordRegistry("test", {"zh": ["你好", "谢谢"]})
        assert reg.match("你好世界").label == "zh"

    def test_word_boundary_matching(self):
        """'search' should not match inside 'research'."""
        reg = KeywordRegistry("test", {"fetch": ["search"]})
        assert reg.match("research the codebase").label is None
        assert reg.match("search online").label == "fetch"


# ============================================================================
# Tier0Engine — Tool Filter
# ============================================================================

class TestTier0ToolFilter:
    @pytest.mark.parametrize("query", [
        "hello", "hi", "hey", "thanks", "thank you", "你好", "谢谢",
        "ok", "sure", "great",
    ])
    def test_conversational_blocked(self, query):
        engine = Tier0Engine()
        tf, max_rounds = engine.classify_tool_filter(query)
        assert tf == ToolFilter.ALL_BLOCKED
        assert max_rounds == 0

    @pytest.mark.parametrize("query", [
        "search online for the latest Python release",
        "帮我搜索一下最新的新闻",
        "what's the weather today",
    ])
    def test_external_fetch(self, query):
        engine = Tier0Engine()
        tf, max_rounds = engine.classify_tool_filter(query)
        assert tf == ToolFilter.LOCAL_BLOCKED
        assert max_rounds == 3

    @pytest.mark.parametrize("query", [
        "How do I implement a binary search tree in Python?",
        "Read the file core/agent/chat_loop.py and find the bug",
        "Refactor the SkillManager class to use dependency injection",
    ])
    def test_default_no_filter(self, query):
        engine = Tier0Engine()
        tf, max_rounds = engine.classify_tool_filter(query)
        assert tf == ToolFilter.NONE
        assert max_rounds == 10

    def test_empty_query(self):
        engine = Tier0Engine()
        tf, _ = engine.classify_tool_filter("")
        assert tf == ToolFilter.NONE

    def test_code_context_suppresses_external_fetch(self):
        """'search' + code keywords → NONE (not EXTERNAL_FETCH)."""
        engine = Tier0Engine()
        tf, _ = engine.classify_tool_filter("search for the bug in this file")
        assert tf == ToolFilter.NONE

    def test_research_not_matched_as_search(self):
        engine = Tier0Engine()
        tf, _ = engine.classify_tool_filter("research the codebase for performance issues")
        assert tf == ToolFilter.NONE

    def test_short_conversational_boosted(self):
        engine = Tier0Engine()
        tf, max_rounds = engine.classify_tool_filter("hello there")
        assert tf == ToolFilter.ALL_BLOCKED
        assert max_rounds == 0

    def test_emoji_query(self):
        engine = Tier0Engine()
        tf, _ = engine.classify_tool_filter("🚀 deploy the app")
        assert tf == ToolFilter.NONE


# ============================================================================
# Tier0Engine — Task Type
# ============================================================================

class TestTier0TaskType:
    @pytest.mark.parametrize("query,expected", [
        ("Please review this PR", TaskType.CODE_REVIEW),
        ("code review for auth module", TaskType.CODE_REVIEW),
        ("refactor the parser", TaskType.CODE_REVIEW),
        ("debug this error", TaskType.DEBUGGING),
        ("fix the crash in login", TaskType.DEBUGGING),
        ("there's a traceback here", TaskType.DEBUGGING),
        ("plan the migration", TaskType.PLANNING),
        ("design a new API", TaskType.PLANNING),
        ("create a roadmap", TaskType.PLANNING),
        ("hello world", TaskType.GENERAL),
        ("what is this?", TaskType.GENERAL),
    ])
    def test_task_type_classification(self, query, expected):
        engine = Tier0Engine()
        assert engine.classify_task_type(query) == expected

    def test_case_insensitive(self):
        engine = Tier0Engine()
        assert engine.classify_task_type("DEBUG this") == TaskType.DEBUGGING
        assert engine.classify_task_type("REVIEW my code") == TaskType.CODE_REVIEW

    def test_first_match_wins(self):
        """When multiple task types match, highest-scoring wins."""
        engine = Tier0Engine()
        result = engine.classify_task_type("review and debug this")
        assert result in (TaskType.CODE_REVIEW, TaskType.DEBUGGING)


# ============================================================================
# Cross-dimension: tool_filter + task_type are independent
# ============================================================================

class TestCrossDimension:
    def test_external_fetch_with_code_review(self):
        """'search online for code review tools' → LOCAL_BLOCKED + CODE_REVIEW."""
        engine = Tier0Engine()
        tf, _ = engine.classify_tool_filter("search online for code review tools")
        # "code" in negative keywords suppresses EXTERNAL_FETCH
        # But "code review" also matches CODE_REVIEW task type
        task = engine.classify_task_type("search online for code review tools")
        assert task == TaskType.CODE_REVIEW

    def test_conversational_is_general(self):
        engine = Tier0Engine()
        tf, _ = engine.classify_tool_filter("hello")
        task = engine.classify_task_type("hello")
        assert tf == ToolFilter.ALL_BLOCKED
        assert task == TaskType.GENERAL

    def test_debug_with_no_filter(self):
        engine = Tier0Engine()
        tf, _ = engine.classify_tool_filter("debug this traceback")
        task = engine.classify_task_type("debug this traceback")
        assert tf == ToolFilter.NONE
        assert task == TaskType.DEBUGGING
