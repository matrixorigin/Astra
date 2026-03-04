"""Unit tests for intent router."""

from core.skills.intent_router import classify_intent


class TestClassifyIntent:
    """Test intent classification."""

    # -- CONVERSATIONAL --

    def test_greeting_en(self):
        result = classify_intent("hello")
        assert result.intent == "CONVERSATIONAL"
        assert result.confidence >= 0.25

    def test_greeting_zh(self):
        result = classify_intent("你好")
        assert result.intent == "CONVERSATIONAL"

    def test_thanks(self):
        result = classify_intent("thank you")
        assert result.intent == "CONVERSATIONAL"

    def test_short_yes(self):
        result = classify_intent("ok")
        assert result.intent == "CONVERSATIONAL"

    # -- EXTERNAL_FETCH --

    def test_web_search(self):
        result = classify_intent("search online for the latest Python release")
        assert result.intent == "EXTERNAL_FETCH"

    def test_fetch_zh(self):
        result = classify_intent("帮我搜索一下最新的新闻")
        assert result.intent == "EXTERNAL_FETCH"

    def test_weather(self):
        result = classify_intent("what's the weather today")
        assert result.intent == "EXTERNAL_FETCH"

    # -- DEFAULT --

    def test_code_question(self):
        result = classify_intent("How do I implement a binary search tree in Python?")
        assert result.intent == "DEFAULT"

    def test_file_operation(self):
        result = classify_intent("Read the file core/agent/chat_loop.py and find the bug")
        assert result.intent == "DEFAULT"

    def test_complex_task(self):
        result = classify_intent("Refactor the SkillManager class to use dependency injection")
        assert result.intent == "DEFAULT"

    # -- Edge cases --

    def test_empty_query(self):
        result = classify_intent("")
        assert result.intent == "DEFAULT"

    def test_search_in_code_context_is_default(self):
        """'search' inside a code-related query must NOT trigger EXTERNAL_FETCH."""
        result = classify_intent("implement a search algorithm for sorting arrays efficiently")
        assert result.intent == "DEFAULT"

    def test_research_not_matched_as_search(self):
        """Word-boundary: 'research' should not match 'search'."""
        result = classify_intent("research the codebase for performance issues")
        assert result.intent == "DEFAULT"

    def test_search_with_file_context_is_default(self):
        """Negative keywords: 'file' suppresses EXTERNAL_FETCH even with 'search'."""
        result = classify_intent("search for the bug in this file")
        assert result.intent == "DEFAULT"

    def test_classification_has_matched_keywords(self):
        result = classify_intent("hello there")
        assert len(result.matched_keywords) > 0
        assert "hello" in result.matched_keywords

    def test_default_has_no_keywords(self):
        result = classify_intent("implement quicksort")
        assert result.matched_keywords == []
        assert result.confidence == 0.0

    def test_long_query_low_keyword_ratio_is_default(self):
        """A long query with one matching keyword should have low confidence → DEFAULT."""
        result = classify_intent(
            "I need to build a comprehensive data pipeline that processes "
            "incoming records, validates schemas, transforms fields, and "
            "writes output to the database with proper error handling"
        )
        assert result.intent == "DEFAULT"

    def test_unicode_emoji_input(self):
        result = classify_intent("🚀 deploy the app")
        assert result.intent == "DEFAULT"
