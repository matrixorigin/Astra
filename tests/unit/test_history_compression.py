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
    
    def test_tier1_recent_3_turns(self):
        """Last 3 turns always in tier1 (full fidelity)."""
        history = [{"user_query": f"q{i}"} for i in range(10)]
        
        result = compress_history_with_references(history, set(), 10000)
        
        assert len(result["tier1"]) == 3
        assert result["tier1"][0]["user_query"] == "q7"
        assert result["tier1"][-1]["user_query"] == "q9"
    
    def test_tier2_preserves_referenced(self):
        """Tier2 keeps full content for referenced events."""
        history = [
            {"user_query": "q1", "tool_results": [
                {"event_id": "evt_1", "tool_name": "read_file", "content": "full content"}
            ]},
            {"user_query": "q2"},
            {"user_query": "q3"},
            {"user_query": "q4"},
        ]
        referenced = {"evt_1"}
        
        result = compress_history_with_references(history, referenced, 10000)
        
        # evt_1 should be in tier2 with full content
        assert len(result["tier2"]) == 1
        assert result["tier2"][0]["tool_results"][0]["content"] == "full content"
    
    def test_tier2_summarizes_unreferenced(self):
        """Tier2 summarizes unreferenced tool results."""
        history = [
            {"user_query": "q1", "tool_results": [
                {"event_id": "evt_2", "tool_name": "read_file", "content": "line1\nline2\nline3", "args": {"path": "test.py"}}
            ]},
            {"user_query": "q2"},
            {"user_query": "q3"},
            {"user_query": "q4"},
        ]
        
        result = compress_history_with_references(history, set(), 10000)
        
        # evt_2 should be summarized
        assert "summary" in result["tier2"][0]["tool_results"][0]
        assert "3 lines" in result["tier2"][0]["tool_results"][0]["summary"]
    
    def test_summarize_tool_result_read_file(self):
        """Test read_file summarization."""
        result = {
            "tool_name": "read_file",
            "content": "line1\nline2\nline3",
            "args": {"path": "config.py"}
        }
        
        summary = _summarize_tool_result(result)
        
        assert "config.py" in summary
        assert "3 lines" in summary
    
    def test_summarize_tool_result_grep(self):
        """Test grep summarization."""
        result = {
            "tool_name": "grep",
            "content": "match1\nmatch2",
            "args": {"pattern": "test"}
        }
        
        summary = _summarize_tool_result(result)
        
        assert "test" in summary
        assert "2 matches" in summary
    
    def test_summarize_text_first_sentence(self):
        """Test text summarization to first sentence."""
        text = "This is the first sentence. This is the second. And third."
        
        summary = _summarize_text(text, max_chars=30)  # Force truncation
        
        # Should be truncated since text is longer than max_chars
        assert len(summary) <= 33  # 30 + "..."
        assert "..." in summary
    
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
            ]
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
