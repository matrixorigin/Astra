"""Tests for ContextBudgetTuner — closed-loop context budget optimization."""

import json
from unittest.mock import Mock, patch, MagicMock

import pytest

from core.evaluation.context_budget_tuner import ContextBudgetTuner


class TestContextBudgetTuner:
    @pytest.fixture
    def db(self):
        return Mock()

    @pytest.fixture
    def tuner(self, db):
        return ContextBudgetTuner(lambda: db)

    # -- diagnose --

    def test_diagnose_flags_low_quality_task_types(self, tuner):
        observations = [
            {"task_type": "debugging", "avg_quality": 2.5, "sample_count": 20, "budgets": []},
            {"task_type": "general", "avg_quality": 4.2, "sample_count": 30, "budgets": []},
        ]
        issues = tuner.diagnose(observations)
        assert len(issues) == 1
        assert issues[0]["task_type"] == "debugging"

    def test_diagnose_skips_insufficient_samples(self, tuner):
        observations = [
            {"task_type": "debugging", "avg_quality": 1.0, "sample_count": 5},
        ]
        assert tuner.diagnose(observations) == []

    # -- propose --

    def test_propose_shifts_budget_from_underutilized_to_overutilized(self, tuner):
        diagnoses = [
            {
                "task_type": "debugging",
                "avg_quality": 2.5,
                "sample_count": 20,
                "utilization": {"logs": 0.95, "code": 0.90, "history": 0.10, "docs": 0.15},
            }
        ]
        proposals = tuner.propose(diagnoses)
        assert proposals is not None
        ratios = proposals["debugging"]
        # Overutilized sections should get more budget
        from core.context.manager import _BUDGET_RATIOS, TaskType

        original = _BUDGET_RATIOS[TaskType.DEBUGGING]
        assert ratios["logs"] >= original["logs"]
        # All ratios should sum to ~1.0
        total = sum(ratios.values())
        assert 0.99 <= total <= 1.01

    def test_propose_returns_none_when_no_diagnoses(self, tuner):
        assert tuner.propose([]) is None

    def test_propose_returns_none_when_no_utilization_data(self, tuner):
        diagnoses = [
            {"task_type": "debugging", "avg_quality": 2.5, "sample_count": 20, "utilization": {}}
        ]
        assert tuner.propose(diagnoses) is None

    # -- validate_and_deploy --

    def test_deploy_writes_to_configs(self, tuner, db):
        proposals = {"debugging": {"logs": 0.45, "code": 0.30, "history": 0.15, "docs": 0.10}}
        with patch("core.evaluation.regression_gate.RegressionGate") as MockGate:
            MockGate.return_value.validate_change.return_value = {"verdict": "pass"}
            result = tuner.validate_and_deploy(proposals)
        assert result["verdict"] == "pass"
        db.execute.assert_called()
        db.commit.assert_called()

    def test_gate_fail_blocks_deploy(self, tuner, db):
        proposals = {"debugging": {"logs": 0.45, "code": 0.30, "history": 0.15, "docs": 0.10}}
        with patch("core.evaluation.regression_gate.RegressionGate") as MockGate:
            MockGate.return_value.validate_change.return_value = {"verdict": "fail"}
            result = tuner.validate_and_deploy(proposals)
        assert result["verdict"] == "fail"
        db.commit.assert_not_called()

    # -- tune (full loop) --

    def test_tune_no_issues_returns_early(self, tuner):
        tuner.observe = Mock(
            return_value=[
                {"task_type": "general", "avg_quality": 4.5, "sample_count": 50, "budgets": []},
            ]
        )
        result = tuner.tune()
        assert result["status"] == "no_issues"

    def test_deploy_rollback_on_error(self, tuner, db):
        db.execute.side_effect = Exception("db error")
        with pytest.raises(Exception, match="db error"):
            tuner._deploy({"debugging": {"logs": 0.5}})
        db.rollback.assert_called()

    # -- _compute_avg_utilization --

    def test_compute_avg_utilization(self):
        budgets = [
            {"code": {"allocated": 100, "used": 80}, "history": {"allocated": 100, "used": 20}},
            {"code": {"allocated": 100, "used": 60}, "history": {"allocated": 100, "used": 40}},
        ]
        util = ContextBudgetTuner._compute_avg_utilization(budgets)
        assert util["code"] == 0.70
        assert util["history"] == 0.30


class TestContextManagerBudgetOverride:
    """Test that ContextManager._load_budget_ratios reads DB overrides."""

    def test_load_budget_ratios_uses_db_override(self):
        from core.context.manager import ContextManager, TaskType

        db = Mock()
        override = {"debugging": {"logs": 0.50, "code": 0.25, "history": 0.15, "docs": 0.10}}
        db.query.return_value.filter.return_value.first.return_value = (json.dumps(override),)

        with (
            patch("core.context.embeddings.EmbeddingService.__init__", return_value=None),
            patch("core.context.prompts.PromptManager.__init__", return_value=None),
            patch("core.context.scorer.RelevanceScorer.__init__", return_value=None),
        ):
            mgr = ContextManager(lambda: db)
            ratios = mgr._load_budget_ratios(TaskType.DEBUGGING)

        assert ratios["logs"] == 0.50

    def test_load_budget_ratios_falls_back_to_defaults(self):
        from core.context.manager import ContextManager, TaskType, _BUDGET_RATIOS

        db = Mock()
        db.query.return_value.filter.return_value.first.return_value = None

        with (
            patch("core.context.embeddings.EmbeddingService.__init__", return_value=None),
            patch("core.context.prompts.PromptManager.__init__", return_value=None),
            patch("core.context.scorer.RelevanceScorer.__init__", return_value=None),
        ):
            mgr = ContextManager(lambda: db)
            ratios = mgr._load_budget_ratios(TaskType.DEBUGGING)

        assert ratios == _BUDGET_RATIOS[TaskType.DEBUGGING]

    def test_load_budget_ratios_caches_result(self):
        from core.context.manager import ContextManager, TaskType

        db = Mock()
        override = {"debugging": {"logs": 0.50, "code": 0.25, "history": 0.15, "docs": 0.10}}
        db.query.return_value.filter.return_value.first.return_value = (json.dumps(override),)

        with (
            patch("core.context.embeddings.EmbeddingService.__init__", return_value=None),
            patch("core.context.prompts.PromptManager.__init__", return_value=None),
            patch("core.context.scorer.RelevanceScorer.__init__", return_value=None),
        ):
            mgr = ContextManager(lambda: db)
            mgr._load_budget_ratios(TaskType.DEBUGGING)
            mgr._load_budget_ratios(TaskType.DEBUGGING)  # second call should use cache

        # Only one DB query despite two calls
        assert db.query.call_count == 1

    def test_load_budget_ratios_logs_on_exception(self):
        from core.context.manager import ContextManager, TaskType

        db = Mock()
        db.query.side_effect = Exception("connection lost")

        with (
            patch("core.context.embeddings.EmbeddingService.__init__", return_value=None),
            patch("core.context.prompts.PromptManager.__init__", return_value=None),
            patch("core.context.scorer.RelevanceScorer.__init__", return_value=None),
            patch("core.context.manager.logger") as mock_logger,
        ):
            mgr = ContextManager(lambda: db)
            ratios = mgr._load_budget_ratios(TaskType.DEBUGGING)
            mock_logger.debug.assert_called_once()

        # Should still return defaults
        from core.context.manager import _BUDGET_RATIOS

        assert ratios == _BUDGET_RATIOS[TaskType.DEBUGGING]
