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
        from api.database import SessionLocal

        sessions_created = []
        sessions_closed = []
        original_factory = SessionLocal

        def tracking_factory():
            s = original_factory()
            sessions_created.append(s)
            original_close = s.close

            def tracked_close():
                sessions_closed.append(s)
                original_close()

            s.close = tracked_close
            return s

        with patch("core.evaluation.drift_pipeline.run_drift_pipeline",
                   side_effect=Exception("drift boom")), \
             patch("core.evaluation.confidence_calibrator.ConfidenceCalibrator.__init__",
                   side_effect=RuntimeError("calibration boom")), \
             patch("core.llm.client.LLMClient.__init__", return_value=None), \
             patch("core.learning.input_face_learner.InputFaceLearner.diagnose_and_fix",
                   return_value=[]), \
             patch("core.skills.self_improving_selector.SelfImprovingSelector.learn_from_failures",
                   return_value={"learned": 0}):

            result = GovernanceTaskRunner._run_eval_daily(tracking_factory)

        # Phase 1 uses db_factory internally (run_drift_pipeline), but we
        # patched it to raise before it calls the factory.
        # Phase 2 calls db_factory() then ConfidenceCalibrator.__init__ raises
        #   → session must still be closed in finally block.
        # Phase 3 calls db_factory() → gets a FRESH session.
        # Phase 4 calls db_factory() → gets a FRESH session.
        #
        # So we expect at least 3 sessions created (Phase 2, 3, 4) and all closed.
        assert len(sessions_created) == 3, (
            f"Expected exactly 3 sessions (one per phase 2-4), got {len(sessions_created)}"
        )
        assert len(sessions_closed) == len(sessions_created), (
            f"Session leak: {len(sessions_created)} created but {len(sessions_closed)} closed"
        )

        # Result should still have Phase 3 and 4 results despite Phase 1+2 failing.
        assert result.get("drift_signals") == 0  # Phase 1 failed
        assert "faces_fixed" in result            # Phase 3 ran
        assert "skills_learned" in result          # Phase 4 ran

    def test_each_phase_gets_distinct_session(self):
        """Verify that Phase 2, 3, 4 each receive a different session object."""
        from api.database import SessionLocal
        from core.evaluation.confidence_calibrator import CalibrationResult

        phase_sessions: dict[str, object] = {}
        original_factory = SessionLocal

        def tracking_factory():
            return original_factory()

        # Capture the session passed to each component's __init__.
        def capture_cal(self_cal, db):
            phase_sessions["phase2"] = db

        def capture_learner(self_learner, db, llm):
            phase_sessions["phase3"] = db

        def capture_selector(self_sel, session, **kwargs):
            phase_sessions["phase4"] = session

        mock_cal_result = CalibrationResult(
            mean_confidence=0.8, mean_quality=0.75,
            calibration_error=0.05, bias=0.05,
            sample_count=100, bucket_errors=[],
        )

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

            GovernanceTaskRunner._run_eval_daily(tracking_factory)

        # All three phases must have received sessions.
        assert "phase2" in phase_sessions
        assert "phase3" in phase_sessions
        assert "phase4" in phase_sessions

        # Each phase must have a DIFFERENT session object.
        # Cannot use id() — sessions are closed between phases so Python may
        # reuse the same memory address.  Use `is not` for identity.
        assert phase_sessions["phase2"] is not phase_sessions["phase3"]
        assert phase_sessions["phase3"] is not phase_sessions["phase4"]
        assert phase_sessions["phase2"] is not phase_sessions["phase4"]
