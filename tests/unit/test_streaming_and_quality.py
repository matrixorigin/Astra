"""Tests for streaming output handler and summary quality evaluator."""

from unittest.mock import MagicMock

import pytest

# StreamingOutputAccumulator tests live in test_streaming_output_handler.py
# to avoid duplication. This file covers SummaryQualityEvaluator only.


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
        from core.agent.summary_quality_evaluator import (
            SummaryQualityEvaluator,
            SummaryQualityResult,
        )

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
