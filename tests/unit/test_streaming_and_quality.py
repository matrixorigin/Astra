"""Tests for streaming output handler."""

import pytest
from unittest.mock import MagicMock, AsyncMock


class TestStreamingOutputAccumulator:
    """Tests for StreamingOutputAccumulator."""

    def test_small_output_returns_full(self):
        """Output under threshold returns full content."""
        from core.agent.streaming_output_handler import StreamingOutputAccumulator
        
        mock_store = MagicMock()
        acc = StreamingOutputAccumulator(
            "shell", "sess1", "user1", mock_store, threshold=1000
        )
        
        acc.accumulate("line1\n")
        acc.accumulate("line2\n")
        result = acc.finalize()
        
        assert result == "line1\nline2\n"
        mock_store.create.assert_not_called()

    def test_large_output_switches_to_storage(self):
        """Output over threshold switches to storage mode."""
        from core.agent.streaming_output_handler import StreamingOutputAccumulator
        
        mock_store = MagicMock()
        mock_memory = MagicMock()
        mock_memory.memory_id = "mem_stream"
        mock_store.create.return_value = mock_memory
        
        acc = StreamingOutputAccumulator(
            "shell", "sess1", "user1", mock_store, threshold=100
        )
        
        # Accumulate over threshold
        acc.accumulate("x" * 150)
        
        assert acc.state.switched_to_storage
        mock_store.create.assert_called_once()

    def test_finalize_returns_summary_with_reference(self):
        """Finalize returns summary + memory reference."""
        from core.agent.streaming_output_handler import StreamingOutputAccumulator
        
        mock_store = MagicMock()
        mock_memory = MagicMock()
        mock_memory.memory_id = "mem_stream"
        mock_store.create.return_value = mock_memory
        
        acc = StreamingOutputAccumulator(
            "shell", "sess1", "user1", mock_store, threshold=100
        )
        
        acc.accumulate("line1\nline2\n" + "x" * 200)
        result = acc.finalize()
        
        assert "memory:mem_stream" in result
        assert "Streaming output" in result

    def test_error_detection_in_summary(self):
        """Errors in output are highlighted in summary."""
        from core.agent.streaming_output_handler import StreamingOutputAccumulator
        
        mock_store = MagicMock()
        mock_memory = MagicMock()
        mock_memory.memory_id = "mem_stream"
        mock_store.create.return_value = mock_memory
        
        acc = StreamingOutputAccumulator(
            "shell", "sess1", "user1", mock_store, threshold=100
        )
        
        acc.accumulate("Building...\nERROR: compilation failed\n" + "x" * 200)
        result = acc.finalize()
        
        assert "error" in result.lower()
        assert "1 error/fail lines" in result


class TestSummaryQualityEvaluator:
    """Tests for SummaryQualityEvaluator."""

    def test_key_info_check_grep(self):
        """Key info check for grep output."""
        from core.agent.summary_quality_evaluator import SummaryQualityEvaluator
        
        mock_llm = MagicMock()
        evaluator = SummaryQualityEvaluator(mock_llm)
        
        original = "file1.py:10:match\nfile2.py:20:match\nfile3.py:30:match"
        summary = "Found 3 matches in 3 files.\nFiles: file1.py, file2.py, file3.py"
        
        score, missing = evaluator._check_key_info("grep", original, summary)
        
        assert score >= 0.8  # Most info preserved
        assert len(missing) == 0

    def test_key_info_missing_files(self):
        """Detects missing file names in summary."""
        from core.agent.summary_quality_evaluator import SummaryQualityEvaluator
        
        mock_llm = MagicMock()
        evaluator = SummaryQualityEvaluator(mock_llm)
        
        original = "file1.py:10:match\nfile2.py:20:match\nfile3.py:30:match"
        summary = "Found 3 matches."  # Missing file names
        
        score, missing = evaluator._check_key_info("grep", original, summary)
        
        assert score < 1.0
        assert any("files" in m for m in missing)

    def test_aggregate_metrics(self):
        """Aggregate metrics calculation."""
        from core.agent.summary_quality_evaluator import SummaryQualityEvaluator, SummaryQualityResult
        
        mock_llm = MagicMock()
        evaluator = SummaryQualityEvaluator(mock_llm)
        
        # Add some results
        evaluator.results = [
            SummaryQualityResult("grep", 1000, 100, 10.0, True, False, 0.9),
            SummaryQualityResult("grep", 2000, 150, 13.3, True, False, 0.8),
            SummaryQualityResult("shell", 500, 100, 5.0, False, True, 0.6),
        ]
        
        metrics = evaluator.get_aggregate_metrics()
        
        assert metrics["total_evaluations"] == 3
        assert metrics["decision_match_rate"] == pytest.approx(2/3)
        assert "grep" in metrics["by_tool"]
        assert "shell" in metrics["by_tool"]
