"""Tests for tiered history compression."""

import pytest
from core.context.history_compression import (
    compress_history_with_references,
    _summarize_tool_result,
    _summarize_text,
)


class TestHistoryCompression:
    """Test 3-tier history compression."""

    def test_short_history_no_compression(self):
        """History ≤3 turns stays in tier1."""
        history = [
            {"user_query": "q1", "llm_response": "r1"},
            {"user_query": "q2", "llm_response": "r2"},
        ]

        result = compress_history_with_references(history, set(), 10000)

        assert len(result["tier1"]) == 2
        assert len(result["tier2"]) == 0
        assert result["tier3"] is None

    def test_tier1_recent_2_turns(self):
        """Last 2 turns always in tier1 (full fidelity) - updated from 3 to 2 for better compression."""
        history = [{"user_query": f"q{i}"} for i in range(10)]

        result = compress_history_with_references(history, set(), 10000)

        # TIER1_RECENT_TURNS changed from 3 to 2 for >50% compression
        assert len(result["tier1"]) == 2
        assert result["tier1"][0]["user_query"] == "q8"
        assert result["tier1"][-1]["user_query"] == "q9"

    def test_tier2_preserves_referenced(self):
        """Tier2 keeps full content for referenced events."""
        history = [
            {
                "user_query": "q1",
                "llm_response": "a1",
                "tool_results": [
                    {"event_id": "evt_1", "tool_name": "read_file", "content": "full content"}
                ],
            },
            {"user_query": "q2", "llm_response": "a2"},
            {"user_query": "q3", "llm_response": "a3"},
            {"user_query": "q4", "llm_response": "a4"},
        ]
        referenced = {"evt_1"}

        result = compress_history_with_references(history, referenced, 10000)

        # evt_1 should be in tier2 with full content
        # Tier2 has first 2 turns (q1, q2), tier1 has last 2 turns (q3, q4)
        assert len(result["tier2"]) == 2
        # First turn has referenced event, should keep full content
        assert result["tier2"][0]["tool_results"][0]["content"] == "full content"
        # LLM response should also be kept full for referenced turn
        assert result["tier2"][0]["llm_response"] == "a1"

    def test_tier2_omits_unreferenced(self):
        """Tier2 omits unreferenced tool results completely (aggressive compression)."""
        history = [
            {
                "user_query": "q1",
                "llm_response": "a1",
                "tool_results": [
                    {
                        "event_id": "evt_2",
                        "tool_name": "read_file",
                        "content": "line1\nline2\nline3",
                        "args": {"path": "test.py"},
                    }
                ],
            },
            {"user_query": "q2", "llm_response": "a2"},
            {"user_query": "q3", "llm_response": "a3"},
            {"user_query": "q4", "llm_response": "a4"},
        ]

        result = compress_history_with_references(history, set(), 10000)

        # evt_2 should be omitted, replaced with summary
        assert len(result["tier2"]) == 2
        assert "summary" in result["tier2"][0]["tool_results"][0]
        assert "omitted" in result["tier2"][0]["tool_results"][0]["summary"]
        # User query and LLM response should be truncated to 80 chars
        assert len(result["tier2"][0]["user_query"]) <= 83  # 80 + "..."
        assert len(result["tier2"][0]["llm_response"]) <= 83

    def test_summarize_tool_result_read_file(self):
        """Test read_file summarization."""
        result = {
            "tool_name": "read_file",
            "content": "line1\nline2\nline3",
            "args": {"path": "config.py"},
        }

        summary = _summarize_tool_result(result)

        assert "config.py" in summary
        assert "3 lines" in summary

    def test_summarize_tool_result_grep(self):
        """Test grep summarization."""
        result = {"tool_name": "grep", "content": "match1\nmatch2", "args": {"pattern": "test"}}

        summary = _summarize_tool_result(result)

        assert "test" in summary
        assert "2 matches" in summary

    def test_summarize_text_first_sentence(self):
        """Test text summarization to first sentence."""
        # Test 1: Text longer than max_chars, should extract first sentence
        text = "This is a long first sentence that contains important information about the system. This is the second sentence with more details. And a third sentence."

        # With max_chars=150, text is longer, should extract first sentence
        summary = _summarize_text(text, max_chars=150)

        # Should return first sentence only
        assert (
            summary
            == "This is a long first sentence that contains important information about the system."
        )
        assert "second sentence" not in summary

        # Test 2: First sentence itself is too long, should truncate
        long_sentence = "This is an extremely long first sentence that exceeds the maximum character limit and should be truncated with ellipsis at the end to indicate there is more content that was cut off."
        summary_truncated = _summarize_text(long_sentence, max_chars=80)

        assert len(summary_truncated) <= 83  # 80 + "..."
        assert "..." in summary_truncated

        # Test 3: Short text, should return as-is
        short_text = "Short text."
        summary_short = _summarize_text(short_text, max_chars=150)
        assert summary_short == short_text

    def test_summarize_text_handles_abbreviations(self):
        """Test that abbreviations don't break sentence detection."""
        text = "Dr. Smith said the value is 3.14. Then he left."

        summary = _summarize_text(text)

        # Should find real sentence boundary after "3.14."
        assert "Dr. Smith" in summary
        assert "3.14" in summary

    def test_summarize_text_truncates_long_sentence(self):
        """Test long first sentence is truncated."""
        text = "A" * 500 + ". Second sentence."

        summary = _summarize_text(text, max_chars=400)

        assert len(summary) <= 403  # 400 + "..."
        assert summary.endswith("...")

    def test_summarize_text_empty_input(self):
        """Test empty input returns empty string."""
        assert _summarize_text("") == ""
        assert _summarize_text(None) == ""

    def test_compress_turn_handles_invalid_input(self):
        """Test compress_turn handles invalid input gracefully."""
        from core.context.history_compression import _compress_turn

        # Invalid turn type
        result = _compress_turn(None, set())
        assert result == {}

        # Missing keys
        result = _compress_turn({}, set())
        assert "user_query" in result
        assert result["user_query"] == ""

    def test_compress_turn_handles_invalid_tool_results(self):
        """Test compress_turn handles invalid tool results."""
        from core.context.history_compression import _compress_turn

        turn = {
            "user_query": "test",
            "tool_results": [
                None,  # Invalid
                "string",  # Invalid type
                {},  # Valid but empty
            ],
        }

        # Should not crash
        result = _compress_turn(turn, set())
        assert isinstance(result, dict)

    def test_tier3_synopsis_created(self):
        """Tier3 synopsis created for long histories."""
        history = [{"user_query": f"query {i}"} for i in range(10)]

        result = compress_history_with_references(history, set(), 10000)

        assert result["tier3"] is not None
        assert "query 0" in result["tier3"]
