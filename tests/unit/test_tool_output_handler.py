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
        """Grep summary includes per-file breakdown and line numbers."""
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
        assert "Per-file breakdown" in summary

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
        from datetime import datetime, timedelta
        mock_memory = MagicMock()
        mock_memory.memory_id = "mem_old"
        mock_memory.metadata = {"tool": "grep"}
        mock_memory.content = "test pattern found in file.py"
        mock_memory.created_at = datetime.now() - timedelta(seconds=60)
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


class TestSummaryRegistry:
    """Tests for summary strategy registry."""

    def test_register_custom_strategy(self):
        """Can register custom summary strategy."""
        from core.agent.tool_output_handler import register_summary_strategy, generate_structured_summary
        
        def custom_summary(output: str) -> str:
            return f"CUSTOM: {len(output)} bytes"
        
        register_summary_strategy("my_tool", custom_summary)
        result = generate_structured_summary("test content", "my_tool")
        assert result == "CUSTOM: 12 bytes"

    def test_json_summary(self):
        """JSON summary extracts keys."""
        from core.agent.tool_output_handler import generate_structured_summary
        import json
        
        data = {"key1": "value1", "key2": "value2", "key3": "value3"}
        result = generate_structured_summary(json.dumps(data), "api_call")
        assert "3 keys" in result
        assert "key1" in result

    def test_file_content_summary(self):
        """File content summary shows head and tail."""
        from core.agent.tool_output_handler import generate_structured_summary
        
        lines = [f"line{i}" for i in range(100)]
        output = '\n'.join(lines)
        result = generate_structured_summary(output, "fs_read")
        assert "100 lines" in result
        assert "line0" in result
        assert "line99" in result


class TestMemoryExpand:
    """Tests for memory expand functionality."""

    def test_expand_full_content(self):
        """Expand returns full content."""
        from core.agent.tool_output_handler import expand_memory_reference
        from unittest.mock import MagicMock
        
        mock_store = MagicMock()
        mock_memory = MagicMock()
        mock_memory.content = "line1\nline2\nline3"
        mock_store.get.return_value = mock_memory
        
        result = expand_memory_reference("mem_123", mock_store)
        assert result == "line1\nline2\nline3"

    def test_expand_with_line_range(self):
        """Expand with line range returns subset."""
        from core.agent.tool_output_handler import expand_memory_reference
        from unittest.mock import MagicMock
        
        mock_store = MagicMock()
        mock_memory = MagicMock()
        mock_memory.content = "line1\nline2\nline3\nline4\nline5"
        mock_store.get.return_value = mock_memory
        
        result = expand_memory_reference("mem_123", mock_store, start_line=2, end_line=4)
        assert "line2" in result
        assert "line3" in result
        assert "line4" in result
        assert "line1" not in result

    def test_expand_with_query_filter(self):
        """Expand with query filters matching lines."""
        from core.agent.tool_output_handler import expand_memory_reference
        from unittest.mock import MagicMock
        
        mock_store = MagicMock()
        mock_memory = MagicMock()
        mock_memory.content = "error: something\ninfo: ok\nerror: another"
        mock_store.get.return_value = mock_memory
        
        result = expand_memory_reference("mem_123", mock_store, query="error")
        assert "2 of 3 lines matching" in result
        assert "error: something" in result
        assert "info: ok" not in result

    def test_expand_not_found(self):
        """Expand returns error for missing memory."""
        from core.agent.tool_output_handler import expand_memory_reference
        from unittest.mock import MagicMock
        
        mock_store = MagicMock()
        mock_store.get.return_value = None
        
        result = expand_memory_reference("mem_missing", mock_store)
        assert "not found" in result


class TestStalenessCheck:
    """Tests for historical result staleness."""

    def test_old_result_rejected(self):
        """Results older than max_age are rejected."""
        from core.agent.tool_output_handler import find_similar_result
        from unittest.mock import MagicMock
        from datetime import datetime, timedelta
        
        mock_retriever = MagicMock()
        mock_memory = MagicMock()
        mock_memory.metadata = {"tool": "grep"}
        mock_memory.content = "test pattern"
        mock_memory.created_at = datetime.now() - timedelta(seconds=600)  # 10 min old
        mock_retriever.retrieve.return_value = [mock_memory]
        
        result = find_similar_result(
            "grep", {"pattern": "test"}, "sess1", "user1", mock_retriever,
            max_age_seconds=300  # 5 min max
        )
        assert result is None  # Rejected due to staleness

    def test_fresh_result_accepted(self):
        """Fresh results are accepted."""
        from core.agent.tool_output_handler import find_similar_result
        from unittest.mock import MagicMock
        from datetime import datetime, timedelta
        
        mock_retriever = MagicMock()
        mock_memory = MagicMock()
        mock_memory.memory_id = "mem_fresh"
        mock_memory.metadata = {"tool": "grep"}
        mock_memory.content = "test pattern found"
        mock_memory.created_at = datetime.now() - timedelta(seconds=60)  # 1 min old
        mock_retriever.retrieve.return_value = [mock_memory]
        
        result = find_similar_result(
            "grep", {"pattern": "test"}, "sess1", "user1", mock_retriever,
            max_age_seconds=300
        )
        assert result is not None
        assert "mem_fresh" in result


class TestSummarizability:
    """Tests for summarizability detection."""

    def test_grep_is_summarizable(self):
        """Grep output is summarizable."""
        from core.agent.tool_output_handler import is_summarizable
        output = "file.py:10:match\nfile.py:20:match"
        assert is_summarizable("grep", output) is True

    def test_fs_read_not_summarizable(self):
        """fs_read is in non-summarizable list."""
        from core.agent.tool_output_handler import is_summarizable
        output = "some content"
        assert is_summarizable("fs_read", output) is False

    def test_code_content_not_summarizable(self):
        """Code content detected and not summarized."""
        from core.agent.tool_output_handler import is_summarizable
        output = "import os\n\ndef main():\n    pass"
        assert is_summarizable("shell", output) is False

    def test_large_code_is_summarizable(self):
        """Large code files (>200 lines) can be summarized."""
        from core.agent.tool_output_handler import is_summarizable
        output = "import os\n" + "\n".join([f"line{i}" for i in range(300)])
        assert is_summarizable("shell", output) is True


class TestFailureModes:
    """Tests for failure mode handling."""

    def test_mo_trustmem_failure_fallback(self):
        """Falls back to truncation when mo-trustmem fails."""
        from core.agent.tool_output_handler import process_tool_output
        from unittest.mock import MagicMock
        
        mock_store = MagicMock()
        mock_store.create.side_effect = Exception("DB connection failed")
        
        output = "x" * 50000
        result = process_tool_output(
            output, "grep", "sess1", "user1", mock_store
        )
        
        assert "truncated" in result
        assert "mo-trustmem unavailable" in result
        assert len(result) < len(output)
