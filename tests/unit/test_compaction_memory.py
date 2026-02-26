"""Tests for context compaction with memory reference preservation."""

import pytest
from core.context.compaction import _clear_old_tool_results, compact, needs_compaction


class TestMemoryReferencePreservation:
    """Tests for preserving memory references during compaction."""

    def test_memory_reference_preserved(self):
        """Memory references are preserved when clearing tool results."""
        messages = [
            {"role": "system", "content": "You are helpful."},
            {"role": "user", "content": "Search for X"},
            {"role": "tool", "content": "Found 100 matches..." + "x" * 500 + "\n\n[Full output (50000 bytes): memory:abc123]"},
            {"role": "assistant", "content": "I found results."},
            {"role": "user", "content": "Show more"},
            {"role": "assistant", "content": "Here you go."},
        ]
        
        result = _clear_old_tool_results(messages.copy(), preserve_recent=2)
        
        # Tool result should be cleared but reference preserved
        tool_msg = result[2]
        assert "memory:abc123" in tool_msg["content"]
        assert "[tool output cleared" in tool_msg["content"]

    def test_no_reference_fully_cleared(self):
        """Tool results without references are fully cleared."""
        messages = [
            {"role": "system", "content": "You are helpful."},
            {"role": "user", "content": "Run command"},
            {"role": "tool", "content": "x" * 500},  # Large output, no reference
            {"role": "assistant", "content": "Done."},
            {"role": "user", "content": "Next"},
            {"role": "assistant", "content": "OK."},
        ]
        
        result = _clear_old_tool_results(messages.copy(), preserve_recent=2)
        
        tool_msg = result[2]
        assert "memory:" not in tool_msg["content"]
        assert "[tool output cleared" in tool_msg["content"]

    def test_recent_messages_not_cleared(self):
        """Recent messages are not cleared."""
        messages = [
            {"role": "system", "content": "You are helpful."},
            {"role": "tool", "content": "Recent large output " + "x" * 500},
            {"role": "assistant", "content": "Done."},
        ]
        
        result = _clear_old_tool_results(messages.copy(), preserve_recent=2)
        
        # Recent tool result should be preserved
        assert "Recent large output" in result[1]["content"]


class TestCompactionIntegration:
    """Integration tests for full compaction flow."""

    def test_compaction_preserves_memory_refs(self):
        """Full compaction preserves memory references."""
        # Build a message chain that needs compaction
        messages = [{"role": "system", "content": "System prompt."}]
        
        # Add many old messages with memory refs
        for i in range(20):
            messages.append({"role": "user", "content": f"Query {i}"})
            messages.append({
                "role": "tool", 
                "content": f"Result {i} " + "x" * 1000 + f"\n[memory:ref{i}]"
            })
            messages.append({"role": "assistant", "content": f"Answer {i}"})
        
        # Compact with small limit
        result = compact(messages, token_limit=5000, preserve_recent=6)
        
        # Check that some memory refs are preserved
        all_content = " ".join(m.get("content", "") for m in result)
        assert "memory:" in all_content
