"""Tests for tool output handler."""

import pytest
from unittest.mock import MagicMock

from core.agent.tool_output_handler import (
    SUMMARY_THRESHOLD,
    generate_structured_summary,
    process_tool_output,
    find_similar_result,
)
from core.memory.types import MemoryType


class TestStructuredSummary:
    """Tests for rule-based structured summary generation."""

    def test_grep_summary_format(self):
        """Grep summary includes file stats and sample."""
        output = "\n".join([
            "file1.py:10:match1",
            "file1.py:20:match2",
            "file2.py:5:match3",
            "file3.py:1:match4",
        ])
        summary = generate_structured_summary(output, "grep")
        
        assert "4 matches" in summary
        assert "3 files" in summary
        assert "file1.py" in summary
        assert "Sample:" in summary

    def test_shell_summary_head_tail(self):
        """Shell summary includes head, tail, and stats."""
        lines = [f"line{i}" for i in range(100)]
        output = "\n".join(lines)
        summary = generate_structured_summary(output, "shell")
        
        assert "100 lines" in summary
        assert "line0" in summary  # head
        assert "line99" in summary  # tail
        assert "..." in summary

    def test_shell_small_output_unchanged(self):
        """Small shell output returned unchanged."""
        output = "small output"
        summary = generate_structured_summary(output, "shell")
        assert summary == output

    def test_default_truncates(self):
        """Unknown tool uses default truncation."""
        output = "x" * 5000
        summary = generate_structured_summary(output, "unknown_tool")
        
        assert len(summary) < 3000
        assert "bytes total" in summary


class TestProcessToolOutput:
    """Tests for process_tool_output with mo-trustmem integration."""

    @pytest.fixture
    def mock_store(self):
        store = MagicMock()
        store.create.return_value = MagicMock(memory_id="mem_123")
        return store

    def test_small_output_returned_directly(self, mock_store):
        """Output under threshold returned without storing."""
        output = "small result"
        result = process_tool_output(
            output, "grep", "sess1", "user1", mock_store
        )
        
        assert result == output
        mock_store.create.assert_not_called()

    def test_large_output_stored_and_summarized(self, mock_store):
        """Large output stored in mo-trustmem with summary returned."""
        output = "x" * 20000  # > 10KB
        result = process_tool_output(
            output, "grep", "sess1", "user1", mock_store
        )
        
        # Should store
        mock_store.create.assert_called_once()
        call_kwargs = mock_store.create.call_args[1]
        assert call_kwargs["memory_type"] == MemoryType.TOOL_RESULT
        assert call_kwargs["session_id"] == "sess1"
        
        # Should return summary + reference
        assert "memory:mem_123" in result
        assert "20000 bytes" in result

    def test_provenance_tracking(self, mock_store):
        """Turn event ID passed for provenance."""
        output = "x" * 20000
        process_tool_output(
            output, "grep", "sess1", "user1", mock_store,
            turn_event_id="event_456"
        )
        
        call_kwargs = mock_store.create.call_args[1]
        assert "event_456" in call_kwargs["source_event_ids"]


class TestFindSimilarResult:
    """Tests for historical result reuse."""

    @pytest.fixture
    def mock_retriever(self):
        return MagicMock()

    def test_no_results_returns_none(self, mock_retriever):
        """No similar results returns None."""
        mock_retriever.retrieve.return_value = []
        
        result = find_similar_result(
            "grep", {"pattern": "test"}, "sess1", "user1", mock_retriever
        )
        
        assert result is None

    def test_matching_result_returns_reference(self, mock_retriever):
        """Matching result returns memory reference."""
        mock_memory = MagicMock()
        mock_memory.memory_id = "mem_old"
        mock_memory.metadata = {"tool": "grep"}
        mock_memory.content = "test pattern found in file.py"
        mock_retriever.retrieve.return_value = [mock_memory]
        
        result = find_similar_result(
            "grep", {"pattern": "test"}, "sess1", "user1", mock_retriever
        )
        
        assert "memory:mem_old" in result
        assert "Reusing" in result

    def test_wrong_tool_returns_none(self, mock_retriever):
        """Result from different tool returns None."""
        mock_memory = MagicMock()
        mock_memory.metadata = {"tool": "shell"}  # Different tool
        mock_retriever.retrieve.return_value = [mock_memory]
        
        result = find_similar_result(
            "grep", {"pattern": "test"}, "sess1", "user1", mock_retriever
        )
        
        assert result is None

    def test_cross_session_search(self, mock_retriever):
        """Cross-session search passes None session_id."""
        mock_retriever.retrieve.return_value = []
        
        find_similar_result(
            "grep", {"pattern": "test"}, "sess1", "user1", mock_retriever,
            cross_session=True
        )
        
        call_kwargs = mock_retriever.retrieve.call_args[1]
        assert call_kwargs["session_id"] is None


class TestToolResultMemoryType:
    """Tests for TOOL_RESULT memory type."""

    def test_tool_result_type_exists(self):
        """TOOL_RESULT type is defined."""
        assert MemoryType.TOOL_RESULT == "tool_result"

    def test_tool_result_is_string_enum(self):
        """TOOL_RESULT works as string."""
        assert MemoryType.TOOL_RESULT.value == "tool_result"


class TestDynamicThreshold:
    """Tests for dynamic threshold computation."""

    def test_default_threshold(self):
        """None remaining_tokens uses default threshold."""
        from core.agent.tool_output_handler import compute_dynamic_threshold, SUMMARY_THRESHOLD
        assert compute_dynamic_threshold(None) == SUMMARY_THRESHOLD

    def test_low_budget_reduces_threshold(self):
        """Low remaining tokens reduces threshold."""
        from core.agent.tool_output_handler import compute_dynamic_threshold, MIN_THRESHOLD
        # 5000 tokens * 4 chars * 0.2 = 4000 bytes
        threshold = compute_dynamic_threshold(5000)
        assert threshold == 4000

    def test_high_budget_increases_threshold(self):
        """High remaining tokens increases threshold (up to max)."""
        from core.agent.tool_output_handler import compute_dynamic_threshold, MAX_THRESHOLD
        # 100000 tokens would give 80000 bytes, but capped at MAX
        threshold = compute_dynamic_threshold(100000)
        assert threshold == MAX_THRESHOLD

    def test_very_low_budget_uses_minimum(self):
        """Very low budget uses minimum threshold."""
        from core.agent.tool_output_handler import compute_dynamic_threshold, MIN_THRESHOLD
        # 1000 tokens * 4 * 0.2 = 800 bytes, but min is 2KB
        threshold = compute_dynamic_threshold(1000)
        assert threshold == MIN_THRESHOLD
