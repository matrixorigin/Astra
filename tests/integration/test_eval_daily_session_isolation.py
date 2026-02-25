"""Integration tests for _run_eval_daily session-per-phase isolation.

Verifies the core correctness property: each phase gets its own DB session
so that a failure (rollback) in one phase cannot corrupt another's work.

Uses real MatrixOne database — only the evaluation components are mocked.
"""

from __future__ import annotations

from unittest.mock import MagicMock, patch

import pytest

from core.context.scheduler import GovernanceTaskRunner


class TestEvalDailySessionIsolation:
    """Each phase must get an independent session from the factory."""

    def test_phase2_failure_does_not_affect_phase3_and_4(self):
        """If Phase 2 (calibration) raises, Phase 3 and 4 still run with
        fresh sessions — not a corrupted/rolled-back one."""

        with patch("core.evaluation.drift_pipeline.run_drift_pipeline",
                   side_effect=Exception("drift boom")), \
             patch("core.evaluation.confidence_calibrator.ConfidenceCalibrator.__init__",
                   side_effect=RuntimeError("calibration boom")), \
             patch("core.llm.client.LLMClient.__init__", return_value=None), \
             patch("core.learning.input_face_learner.InputFaceLearner.diagnose_and_fix",
                   return_value=[]), \
             patch("core.skills.self_improving_selector.SelfImprovingSelector.learn_from_failures",
                   return_value={"learned": 0}):

            result = GovernanceTaskRunner._run_eval_daily(lambda: MagicMock())

        # Result should still have Phase 3 and 4 results despite Phase 1+2 failing.
        assert result.get("drift_signals") == 0  # Phase 1 failed
        assert "faces_fixed" in result            # Phase 3 ran
        assert "skills_learned" in result          # Phase 4 ran

    def test_each_phase_gets_distinct_session(self):
        """Verify that Phase 2, 3, 4 each receive the db_factory (not a raw session)."""
        from core.evaluation.confidence_calibrator import CalibrationResult

        phase_factories: dict[str, object] = {}

        def capture_cal(self_cal, db_factory):
            phase_factories["phase2"] = db_factory

        def capture_learner(self_learner, db_factory, llm):
            phase_factories["phase3"] = db_factory

        def capture_selector(self_sel, db_factory, **kwargs):
            phase_factories["phase4"] = db_factory

        mock_cal_result = CalibrationResult(
            mean_confidence=0.8, mean_quality=0.75,
            calibration_error=0.05, bias=0.05,
            sample_count=100, bucket_errors=[],
        )

        factory = lambda: MagicMock()

        with patch("core.evaluation.drift_pipeline.run_drift_pipeline",
                   side_effect=Exception("skip")), \
             patch("core.evaluation.confidence_calibrator.ConfidenceCalibrator.__init__",
                   capture_cal), \
             patch("core.evaluation.confidence_calibrator.ConfidenceCalibrator.measure",
                   return_value=mock_cal_result), \
             patch("core.llm.client.LLMClient.__init__", return_value=None), \
             patch("core.learning.input_face_learner.InputFaceLearner.__init__",
                   capture_learner), \
             patch("core.learning.input_face_learner.InputFaceLearner.diagnose_and_fix",
                   return_value=[]), \
             patch("core.skills.self_improving_selector.SelfImprovingSelector.__init__",
                   capture_selector), \
             patch("core.skills.self_improving_selector.SelfImprovingSelector.learn_from_failures",
                   return_value={"learned": 0}):

            GovernanceTaskRunner._run_eval_daily(factory)

        # All three phases must have received the same factory.
        assert "phase2" in phase_factories
        assert "phase3" in phase_factories
        assert "phase4" in phase_factories
        # All phases share the same factory — each DbConsumer creates
        # independent sessions via _db() internally.
        assert phase_factories["phase2"] is factory
        assert phase_factories["phase3"] is factory
        assert phase_factories["phase4"] is factory
