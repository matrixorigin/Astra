"""Tests for streaming output handler."""

import pytest
from unittest.mock import MagicMock

from core.memory.types import Memory, MemoryType


class TestStreamingOutputAccumulator:
    """Tests for StreamingOutputAccumulator."""

    @pytest.fixture
    def mock_service(self):
        svc = MagicMock()
        mock_memory = MagicMock()
        mock_memory.memory_id = "mem_stream"
        svc.create_memory.return_value = mock_memory
        return svc

    def test_small_output_returns_full(self, mock_service):
        """Output under threshold returns full content."""
        from core.agent.streaming_output_handler import StreamingOutputAccumulator
        
        acc = StreamingOutputAccumulator(
            "shell", "sess1", "user1", mock_service, threshold=1000
        )
        
        acc.accumulate("line1\n")
        acc.accumulate("line2\n")
        result = acc.finalize()
        
        assert result == "line1\nline2\n"
        mock_service.create_memory.assert_not_called()

    def test_large_output_switches_to_storage(self, mock_service):
        """Output over threshold switches to storage mode."""
        from core.agent.streaming_output_handler import StreamingOutputAccumulator
        
        acc = StreamingOutputAccumulator(
            "shell", "sess1", "user1", mock_service, threshold=100
        )
        
        acc.accumulate("x" * 150)
        
        assert acc.state.switched_to_storage
        mock_service.create_memory.assert_called_once()

    def test_finalize_returns_summary_with_reference(self, mock_service):
        """Finalize returns summary + memory reference."""
        from core.agent.streaming_output_handler import StreamingOutputAccumulator
        
        acc = StreamingOutputAccumulator(
            "shell", "sess1", "user1", mock_service, threshold=100
        )
        
        acc.accumulate("line1\nline2\n" + "x" * 200)
        result = acc.finalize()
        
        assert "memory:mem_stream" in result
        assert "Streaming output" in result

    def test_error_detection_in_summary(self, mock_service):
        """Errors in output are highlighted in summary."""
        from core.agent.streaming_output_handler import StreamingOutputAccumulator
        
        acc = StreamingOutputAccumulator(
            "shell", "sess1", "user1", mock_service, threshold=100
        )
        
        acc.accumulate("Building...\nERROR: compilation failed\n" + "x" * 200)
        result = acc.finalize()
        
        assert "error" in result.lower()
        assert "1 error/fail lines" in result
