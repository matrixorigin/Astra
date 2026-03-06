"""Tests for ModelRouter complexity classification."""

from unittest.mock import MagicMock


class TestModelRouterClassification:
    """Verify classify_complexity LLM call signature and fallback."""

    def test_classify_passes_user_id_and_task_hint(self):
        from core.agents.routing import ModelRouter

        mock_llm = MagicMock()
        mock_response = MagicMock()
        mock_response.content = "simple"
        mock_llm.chat.return_value = mock_response

        router = ModelRouter(db_factory=MagicMock(), llm_client=mock_llm)

        router.classify_complexity("general", "test query")

        call_kwargs = mock_llm.chat.call_args[1]
        assert call_kwargs["user_id"] == "routing"
        assert call_kwargs["task_hint"] == "routing"
