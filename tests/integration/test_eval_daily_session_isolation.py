"""Integration tests for _run_eval_daily session-per-phase isolation.

Verifies the core correctness property: each phase's DbConsumer creates
independent sessions via _db(), so a failure (rollback) in one phase
cannot corrupt another's work.

Uses real MatrixOne database — only the evaluation components are mocked.
"""

from __future__ import annotations

from unittest.mock import MagicMock, patch

import pytest

from core.context.scheduler import GovernanceTaskRunner


class TestEvalDailySessionIsolation:
    """Each phase must get an independent session from the factory."""

    def test_phase2_failure_does_not_affect_phase3_and_4(self):
        """If Phase 2 (calibration) raises, Phase 3 and 4 still succeed.

        DbConsumer._db() creates a fresh session per call, so Phase 2's
        exception and rollback cannot affect Phase 3/4's sessions.
        """
        from api.database import SessionLocal

        # Track sessions created by the factory to verify isolation
        sessions_created: list = []
        sessions_closed: list = []

        def tracking_factory():
            s = SessionLocal()
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

        # Phase 3 and 4 must have run despite Phase 1+2 failing.
        assert result.get("drift_signals") == 0  # Phase 1 failed
        assert "faces_fixed" in result            # Phase 3 ran
        assert "skills_learned" in result          # Phase 4 ran

        # Every session created by the factory must be closed (no leaks).
        # This catches real resource leaks: e.g. a phase that calls the factory
        # but an exception path bypasses _db()'s finally clause.
        assert len(sessions_closed) == len(sessions_created), (
            f"Session leak: {len(sessions_created)} created but {len(sessions_closed)} closed"
        )

    def test_all_phases_share_factory_but_get_independent_sessions(self):
        """All DbConsumer constructors receive the same factory, but each
        _db() call inside their methods creates a distinct session.

        This verifies the DbConsumer contract: factory is shared,
        sessions are independent.
        """
        from api.database import SessionLocal

        sessions_created: list = []

        def tracking_factory():
            s = SessionLocal()
            sessions_created.append(s)
            return s

        # Let constructors run (they call _db() internally for schema checks etc.)
        # but mock the heavy operations that would fail without real data.
        with patch("core.evaluation.drift_pipeline.run_drift_pipeline",
                   side_effect=Exception("skip")), \
             patch("core.evaluation.confidence_calibrator.ConfidenceCalibrator.measure",
                   return_value=MagicMock(calibration_error=0.05)), \
             patch("core.learning.input_face_learner.InputFaceLearner.diagnose_and_fix",
                   return_value=[]), \
             patch("core.skills.self_improving_selector.SelfImprovingSelector.learn_from_failures",
                   return_value={"learned": 0}):

            GovernanceTaskRunner._run_eval_daily(tracking_factory)

        # Multiple sessions must have been created (one per _db() call).
        assert len(sessions_created) >= 2, (
            f"Expected multiple independent sessions, got {len(sessions_created)}"
        )
        # All sessions must be distinct objects (not reused).
        session_ids = [id(s) for s in sessions_created]
        assert len(set(session_ids)) == len(session_ids), "Sessions must be distinct objects"
