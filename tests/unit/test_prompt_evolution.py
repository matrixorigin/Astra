"""Tests for prompt evolution."""

from unittest.mock import Mock

import pytest

from core.evaluation.prompt_evolution import PromptEvolver, PromptVariant


def _mock_db():
    return Mock()


class TestPromptEvolver:
    def test_create_variant(self):
        db = _mock_db()
        db.execute.side_effect = [
            Mock(fetchone=Mock(return_value=(0,))),  # Get max version
            None,  # Insert variant
        ]

        evolver = PromptEvolver(lambda: db)
        variant = evolver.create_variant(
            prompt_template_id="template-1",
            content="New prompt content",
            description="Improved clarity",
        )

        assert variant.prompt_template_id == "template-1"
        assert variant.version == 1
        assert variant.content == "New prompt content"

    def test_evaluate_variant(self):
        db = _mock_db()
        mock_execute = Mock()
        mock_execute.fetchone.return_value = ("New prompt",)
        db.execute.return_value = mock_execute

        evolver = PromptEvolver(lambda: db)

        def mock_replay(session_id, content):
            return 4.5

        score = evolver.evaluate_variant(
            variant_id="var-1",
            golden_sessions=["sess-1", "sess-2"],
            replay_fn=mock_replay,
        )

        assert score == 4.5

    def test_promote_variant(self):
        db = _mock_db()
        db.execute.side_effect = [
            Mock(fetchone=Mock(return_value=("New prompt", 2))),  # Get variant (content, version)
            None,  # Update template
        ]

        evolver = PromptEvolver(lambda: db)
        result = evolver.promote_variant("var-1", "template-1")

        assert result["promoted"] is True
        db.execute.assert_called()

    def test_get_best_variant(self):
        db = _mock_db()
        db.execute.return_value = Mock(fetchone=Mock(return_value=("var-1", 2, "Best prompt", 4.8)))

        evolver = PromptEvolver(lambda: db)
        variant = evolver.get_best_variant("template-1")

        assert variant is not None
        assert variant.variant_id == "var-1"
        assert variant.quality_score == 4.8
