"""Integration tests for prompt assembly with compression."""

import pytest
from core.context.prompt_integration import (
    integrate_compression_into_prompt,
    _format_history_simple,
    _format_compressed_history,
)


class TestPromptIntegration:
    """Test integration of compression into prompt assembly."""
    
    def test_short_history_no_compression(self):
        """Short history (≤3 turns) bypasses compression."""
        history = [
            {"user_query": "Hello", "llm_response": "Hi there"},
            {"user_query": "How are you?", "llm_response": "I'm good"},
        ]
        
        result = integrate_compression_into_prompt(
            history, "", [], elastic_budget=10000, enable_compression=True
        )
        
        assert "Turn 1:" in result
        assert "Hello" in result
    
    def test_compression_disabled(self):
        """Compression disabled returns simple format."""
        history = [{"user_query": f"q{i}"} for i in range(10)]
        
        result = integrate_compression_into_prompt(
            history, "", [], elastic_budget=10000, enable_compression=False
        )
        
        assert "Turn 1:" in result
        assert "Turn 10:" in result
    
    def test_compression_with_references(self):
        """Compression preserves referenced content."""
        history = [
            {"user_query": "Read config", "tool_results": [
                {"event_id": "evt_1", "tool_name": "read_file", "content": "DATABASE_URL=postgres", "args": {"path": "config.py"}}
            ]},
            {"user_query": "q2"},
            {"user_query": "q3"},
            {"user_query": "q4"},
        ]
        current_response = "In config.py, DATABASE_URL is set..."
        
        result = integrate_compression_into_prompt(
            history, current_response, [], elastic_budget=10000, enable_compression=True
        )
        
        # Should have compressed content (no section headers in new format)
        # Verify referenced content is preserved
        assert "read_file: DATABASE_URL=postgres" in result
        # Verify recent turns are included
        assert "q3" in result or "q4" in result
    
    def test_format_simple(self):
        """Test simple formatting."""
        history = [
            {"user_query": "Hello", "llm_response": "Hi"},
        ]
        
        result = _format_history_simple(history)
        
        assert "Turn 1:" in result
        assert "User: Hello" in result
        assert "Assistant: Hi" in result
    
    def test_format_compressed_with_all_tiers(self):
        """Test formatting with all 3 tiers."""
        compressed = {
            "tier3": "Session started with initial query.",
            "tier2": [{
                "user_query": "Middle query",
                "tool_results": [{"summary": "read_file(test.py) → 10 lines"}],
                "llm_response": "Middle response"
            }],
            "tier1": [{
                "user_query": "Recent query",
                "llm_response": "Recent response"
            }]
        }
        
        result = _format_compressed_history(compressed)
        
        # New format doesn't use section headers (removed for compression efficiency)
        # Verify content is present
        assert "Session started with initial query" in result  # tier3
        assert "Middle query" in result  # tier2
        assert "Recent query" in result  # tier1
        assert "read_file(test.py)" in result  # tool result summary
    
    def test_format_compressed_tier1_only(self):
        """Test formatting with only tier1 (short history)."""
        compressed = {
            "tier1": [{"user_query": "q1", "llm_response": "r1"}],
            "tier2": [],
            "tier3": None
        }
        
        result = _format_compressed_history(compressed)
        
        # New format doesn't use section headers
        assert "q1" in result
        assert "r1" in result
        # Verify no synopsis (tier3 is None)
        assert result.count("User:") == 1  # Only one turn
