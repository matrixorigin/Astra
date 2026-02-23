"""E2E tests for P3 Data Versioning integration — experiment→gate, knowledge→regression."""

import os
from datetime import datetime
from unittest.mock import MagicMock

import pytest
from sqlalchemy import text
from sqlalchemy.orm import Session

from api.database import get_db_session
from core.data_versioning import (
    ExperimentConfig,
    ExperimentStatus,
    KnowledgeRegression,
    PromptExperiment,
    PromptVariant,
)

TEST_DB = os.environ["MATRIXONE_DATABASE"]


@pytest.fixture
def db():
    return next(get_db_session())


# ---------------------------------------------------------------------------
# P3-1: PromptExperiment.complete_experiment → RegressionGate
# ---------------------------------------------------------------------------


class TestExperimentGateIntegration:
    """Experiment winner must pass regression gate before promotion."""

    def _create_and_populate_experiment(self, db: Session, exp_id: str) -> PromptExperiment:
        """Helper: create experiment, record results so v1 wins."""
        exp = PromptExperiment(db, source_db=TEST_DB)
        config = ExperimentConfig(
            experiment_id=exp_id,
            name="Gate test",
            description="test",
            skill_name="test_skill",
            baseline_variant=PromptVariant(
                variant_id="baseline", name="Baseline", system_prompt="You are helpful.",
            ),
            test_variants=[
                PromptVariant(
                    variant_id="v1", name="Variant 1", system_prompt="You are very helpful.",
                ),
            ],
            sample_size=10,
        )
        exp.create_experiment(config)
        exp.start_experiment(exp_id)

        # Record results: v1 clearly better than baseline
        for i in range(20):
            exp.record_variant_result(
                exp_id, "baseline", f"s{i}", f"e{i}",
                accuracy=0.70, latency_ms=100, cost_usd=0.001, satisfaction=0.7,
            )
            exp.record_variant_result(
                exp_id, "v1", f"s{i}", f"e{i}",
                accuracy=0.92, latency_ms=90, cost_usd=0.001, satisfaction=0.9,
            )
        return exp

    def test_complete_without_gate_promotes_winner(self, db: Session):
        """Without gate, winner is promoted directly."""
        exp_id = f"exp_no_gate_{datetime.utcnow().strftime('%H%M%S%f')}"
        exp = self._create_and_populate_experiment(db, exp_id)
        try:
            winner = exp.complete_experiment(exp_id)
            assert winner == "v1"

            row = db.execute(text(f"""
                SELECT status, winner_variant_id FROM {exp_id}.experiment_config
                WHERE experiment_id = :eid
            """), {"eid": exp_id}).fetchone()
            assert row[0] == ExperimentStatus.COMPLETED.value
            assert row[1] == "v1"
        finally:
            exp.cleanup_experiment(exp_id)

    def test_complete_with_gate_pass(self, db: Session):
        """Gate passes → winner promoted."""
        exp_id = f"exp_gp_{datetime.utcnow().strftime('%H%M%S%f')}"
        exp = self._create_and_populate_experiment(db, exp_id)

        gate = MagicMock()
        gate.validate_change.return_value = {"verdict": "pass", "reason": "ok"}

        try:
            winner = exp.complete_experiment(exp_id, regression_gate=gate)
            assert winner == "v1"
            gate.validate_change.assert_called_once()

            # Verify the change_content passed to gate contains the winner prompt
            call_kwargs = gate.validate_change.call_args
            assert call_kwargs.kwargs["change_content"]["content"] == "You are very helpful."
        finally:
            exp.cleanup_experiment(exp_id)

    def test_complete_with_gate_fail(self, db: Session):
        """Gate fails → winner NOT promoted, status = gate_failed."""
        exp_id = f"exp_gf_{datetime.utcnow().strftime('%H%M%S%f')}"
        exp = self._create_and_populate_experiment(db, exp_id)

        gate = MagicMock()
        gate.validate_change.return_value = {
            "verdict": "fail",
            "reason": "error_rate 12% > threshold 5%",
        }

        try:
            result = exp.complete_experiment(exp_id, regression_gate=gate)
            assert result == "gate_failed"

            row = db.execute(text(f"""
                SELECT status FROM {exp_id}.experiment_config
                WHERE experiment_id = :eid
            """), {"eid": exp_id}).fetchone()
            assert row[0] == "gate_failed"
        finally:
            exp.cleanup_experiment(exp_id)


# ---------------------------------------------------------------------------
# P3-2: Memory pipeline quarantine → KnowledgeRegression signal
# ---------------------------------------------------------------------------


class TestKnowledgeRegressionIntegration:
    """KnowledgeRegression queries work against real MatrixOne."""

    def test_pipeline_no_quarantine_for_clean_user(self, db: Session):
        """Clean user — no quarantine, no errors."""
        from core.memory.pipeline import run_memory_pipeline

        result = run_memory_pipeline(db, user_id=f"clean_{datetime.utcnow().strftime('%H%M%S%f')}")
        assert result.quarantined == 0

    def test_knowledge_regression_detect_change_impact(self, db: Session):
        """KnowledgeRegression.detect_knowledge_change_impact returns signal."""
        kr = KnowledgeRegression(db, source_db=TEST_DB)
        signal = kr.detect_knowledge_change_impact(
            knowledge_domain="test_domain",
            changed_at=datetime.utcnow(),
        )
        # No matching data → confidence 0
        assert signal.confidence == 0.0
        assert signal.regression_type.value == "knowledge_changed"
