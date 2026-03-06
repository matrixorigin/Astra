"""Tests for SummaryQualityEvaluator LLM call signature."""

from unittest.mock import MagicMock


class TestSummaryQualityEvaluatorLLMCall:
    """Verify _get_decision passes correct parameters to llm.chat()."""

    def test_get_decision_passes_user_id_and_task_hint(self):
        from core.agent.summary_quality_evaluator import SummaryQualityEvaluator

        mock_llm = MagicMock()
        mock_response = MagicMock()
        mock_response.content = "USE_TOOL: grep"
        mock_llm.chat.return_value = mock_response

        evaluator = SummaryQualityEvaluator(mock_llm)
        evaluator._get_decision("some content", "find files")

        call_kwargs = mock_llm.chat.call_args[1]
        assert call_kwargs["user_id"] == "summary_quality_evaluator"
        assert call_kwargs["task_hint"] == "summary_quality_eval"
