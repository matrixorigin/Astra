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

    def test_batch_chunking_over_limit(self, db: Session):
        """Batch > BATCH_LIMIT (100) is chunked into multiple statements, single commit."""
        exp_id = f"exp_chunk_{datetime.utcnow().strftime('%H%M%S%f')}"
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


class TestSkillSelectionEventsFixes:
    """E2E: _record_selection writes skill_version, selection_history returns
    skill_name, detect_skill_update_regression queries skill_selection_events."""

    def _seed_selection(self, db: Session, event_id: str, session_id: str,
                        skill_name: str, skill_version: str | None,
                        execution_success: int = 1, feedback: int | None = None):
        """Insert a skill_selection_events row with version + execution data."""
        db.execute(text("""
            INSERT INTO skill_selection_events
                (event_id, session_id, user_query, selected_skills,
                 skill_name, skill_version, selection_method,
                 execution_success, user_feedback_score, created_at)
            VALUES (:eid, :sid, 'test query', :skills,
                    :sn, :sv, 'rule',
                    :es, :fb, NOW())
        """), {
            "eid": event_id, "sid": session_id,
            "skills": f'["{skill_name}"]',
            "sn": skill_name, "sv": skill_version,
            "es": execution_success, "fb": feedback,
        })
        db.commit()

    # -- Fix 1: _record_selection writes skill_version from registry --------

    def test_record_selection_writes_skill_version(self, db: Session):
        """Pipeline._record_selection resolves and persists skill_version."""
        from unittest.mock import MagicMock
        from core.skills.pipeline import SkillPipeline

        # Register a skill so version can be resolved
        skill_id = f"test_ver_{datetime.utcnow().strftime('%H%M%S%f')}"
        db.execute(text("""
            INSERT INTO skill_registry (skill_id, skill_name, version, is_active, created_at)
            VALUES (:sid, :sn, '2.1.0', 1, NOW())
        """), {"sid": f"{skill_id}@2.1.0", "sn": skill_id})
        db.commit()

        pipe = SkillPipeline(lambda: db, MagicMock(), audit=True, learning=False)
        tools = [{"type": "function", "function": {"name": skill_id, "description": "t", "parameters": {}}}]
        try:
            eid = pipe._record_selection("q", "sess_ver", tools, "rule")
            row = db.execute(text(
                "SELECT skill_name, skill_version FROM skill_selection_events WHERE event_id = :eid"
            ), {"eid": eid}).fetchone()
            assert row[0] == skill_id
            assert row[1] == "2.1.0"
        finally:
            db.execute(text("DELETE FROM skill_selection_events WHERE session_id = 'sess_ver'"), {})
            db.execute(text("DELETE FROM skill_registry WHERE skill_id = :sid"), {"sid": f"{skill_id}@2.1.0"})
            db.commit()

    # -- Fix 2: selection_history returns skill_name -----------------------

    def test_selection_history_includes_skill_name(self, db: Session):
        """selection_history() returns skill_name in each dict."""
        from unittest.mock import MagicMock
        from core.skills.pipeline import SkillPipeline

        eid = f"hist_{datetime.utcnow().strftime('%H%M%S%f')}"
        sid = f"sess_hist_{eid}"
        self._seed_selection(db, eid, sid, "my_skill", "1.0.0")

        pipe = SkillPipeline(lambda: db, MagicMock(), audit=False, learning=False)
        try:
            history = pipe.selection_history(session_id=sid)
            assert len(history) == 1
            assert history[0]["skill_name"] == "my_skill"
            assert "skill_name" in history[0]
        finally:
            db.execute(text("DELETE FROM skill_selection_events WHERE event_id = :eid"), {"eid": eid})
            db.commit()

    # -- Fix 3: detect_skill_update_regression uses skill_selection_events --

    def test_detect_skill_update_regression_detects_drop(self, db: Session):
        """Regression detected when new version has lower success rate."""
        ts = datetime.utcnow().strftime('%H%M%S%f')
        skill = f"regr_skill_{ts}"

        # Old version: 5 successes
        for i in range(5):
            self._seed_selection(db, f"old_{ts}_{i}", f"s_old_{i}", skill, "1.0.0",
                                 execution_success=1)
        # New version: 5 failures
        for i in range(5):
            self._seed_selection(db, f"new_{ts}_{i}", f"s_new_{i}", skill, "2.0.0",
                                 execution_success=0)

        kr = KnowledgeRegression(lambda: db, source_db=TEST_DB)
        try:
            signal = kr.detect_skill_update_regression(skill, "1.0.0", "2.0.0")
            assert signal.confidence > 0  # regression detected
            assert signal.metadata["before_success_rate"] == 1.0
            assert signal.metadata["after_success_rate"] == 0.0
            assert signal.metadata["quality_drop"] == 1.0
            assert signal.affected_sessions == 10
        finally:
            db.execute(text("DELETE FROM skill_selection_events WHERE skill_name = :sn"), {"sn": skill})
            db.commit()

    def test_detect_skill_update_regression_no_regression(self, db: Session):
        """No regression when both versions succeed equally."""
        ts = datetime.utcnow().strftime('%H%M%S%f')
        skill = f"ok_skill_{ts}"

        for i in range(3):
            self._seed_selection(db, f"ov_{ts}_{i}", f"so_{i}", skill, "1.0.0", execution_success=1)
            self._seed_selection(db, f"nv_{ts}_{i}", f"sn_{i}", skill, "2.0.0", execution_success=1)

        kr = KnowledgeRegression(lambda: db, source_db=TEST_DB)
        try:
            signal = kr.detect_skill_update_regression(skill, "1.0.0", "2.0.0")
            assert signal.confidence == 0.0  # no regression
        finally:
            db.execute(text("DELETE FROM skill_selection_events WHERE skill_name = :sn"), {"sn": skill})
            db.commit()
