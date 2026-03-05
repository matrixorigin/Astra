"""E2E tests for P3 Data Versioning integration — experiment→gate, knowledge→regression."""

import os
from datetime import datetime, timezone
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
        exp = PromptExperiment(lambda: db, source_db=TEST_DB)
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

        # Record results in batch: v1 clearly better than baseline
        baseline_results = [
            {"variant_id": "baseline", "session_id": f"s{i}", "event_id": f"e{i}",
             "accuracy": 0.70, "latency_ms": 100, "cost_usd": 0.001, "satisfaction": 0.7}
            for i in range(20)
        ]
        v1_results = [
            {"variant_id": "v1", "session_id": f"s{i}", "event_id": f"e{i}",
             "accuracy": 0.92, "latency_ms": 90, "cost_usd": 0.001, "satisfaction": 0.9}
            for i in range(20)
        ]
        exp.record_variant_results_batch(exp_id, baseline_results + v1_results)
        return exp

    def test_complete_without_gate_promotes_winner(self, db: Session):
        """Without gate, winner is promoted directly."""
        exp_id = f"exp_no_gate_{datetime.now(timezone.utc).strftime('%H%M%S%f')}"
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
        exp_id = f"exp_gp_{datetime.now(timezone.utc).strftime('%H%M%S%f')}"
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
        exp_id = f"exp_gf_{datetime.now(timezone.utc).strftime('%H%M%S%f')}"
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

    def test_batch_chunking_over_limit(self, db: Session):
        """Batch > BATCH_LIMIT (100) is chunked into multiple statements, single commit."""
        exp_id = f"exp_chunk_{datetime.now(timezone.utc).strftime('%H%M%S%f')}"
        exp = PromptExperiment(lambda: db, source_db=TEST_DB)
        config = ExperimentConfig(
            experiment_id=exp_id,
            name="Chunk test",
            description="test",
            skill_name="test_skill",
            baseline_variant=PromptVariant(variant_id="b", name="B", system_prompt="x"),
            test_variants=[],
            sample_size=10,
        )
        exp.create_experiment(config)
        exp.start_experiment(exp_id)

        # 150 rows — exceeds BATCH_LIMIT=100, must be chunked
        results = [
            {"variant_id": "b", "session_id": f"s{i}", "event_id": f"e{i}",
             "accuracy": 0.8, "latency_ms": 50, "cost_usd": 0.001, "satisfaction": 0.8}
            for i in range(150)
        ]
        try:
            inserted = exp.record_variant_results_batch(exp_id, results)
            assert inserted == 150

            row = db.execute(text(f"SELECT COUNT(*) FROM {exp_id}.variant_results")).fetchone()
            assert row[0] == 150
        finally:
            exp.cleanup_experiment(exp_id)


# ---------------------------------------------------------------------------
# P4: skill_selection_events — skill_name/skill_version/selection_history
# ---------------------------------------------------------------------------


