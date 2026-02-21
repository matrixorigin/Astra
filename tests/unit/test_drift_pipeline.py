"""Tests for Drift Auto-Correction Pipeline."""

from datetime import datetime, timezone
from unittest.mock import MagicMock, patch

import pytest

from core.evaluation.drift_detector import (
    CorrectionAction,
    DriftCorrector,
    DriftDetector,
    DriftReport,
    DriftSeverity,
    DriftSignal,
)
from core.evaluation.drift_pipeline import PipelineResult, run_drift_pipeline


def _signal(
    model="gpt-4", severity=DriftSeverity.SIGNIFICANT, delta=-0.6,
    template_id=None,
) -> DriftSignal:
    return DriftSignal(
        model=model, template_id=template_id,
        current_avg=3.4, previous_avg=3.4 - delta,
        week_delta=delta, severity=severity,
        sample_count=20, detected_at=datetime.now(timezone.utc),
    )


class TestPipelineNoSignals:
    def test_no_drift_returns_empty(self):
        db = MagicMock()
        db_factory = MagicMock(return_value=db)

        with patch.object(DriftDetector, "detect", return_value=[]):
            result = run_drift_pipeline(db_factory)

        assert result.signals_detected == 0
        assert result.corrections_applied == 0
        assert result.error is None
        db.close.assert_called_once()


class TestPipelineWithSignals:
    def test_detect_confirm_correct_flow(self):
        """Full pipeline: detect → confirm → correct."""
        db = MagicMock()
        db_factory = MagicMock(return_value=db)
        sig = _signal(severity=DriftSeverity.SIGNIFICANT, delta=-0.6)

        report = DriftReport(
            signals=[sig], confirmed=[sig],
            corrections=[{
                "model": "gpt-4", "action": CorrectionAction.OPTIMIZE_PROMPT.value,
                "severity": "significant", "week_delta": -0.6,
            }],
            created_at=datetime.now(timezone.utc),
        )

        with (
            patch.object(DriftDetector, "detect", return_value=[sig]),
            patch.object(DriftCorrector, "confirm_and_correct", return_value=report),
            patch("core.evaluation.drift_pipeline._try_build_regression_gate", return_value=None),
            patch("core.evaluation.drift_pipeline._try_build_prompt_optimizer", return_value=None),
        ):
            result = run_drift_pipeline(db_factory)

        assert result.signals_detected == 1
        assert result.signals_confirmed == 1
        assert result.corrections_applied == 1
        assert result.actions[0]["action"] == "optimize_prompt"

    def test_escalate_human_not_counted_as_correction(self):
        db = MagicMock()
        db_factory = MagicMock(return_value=db)
        sig = _signal()

        report = DriftReport(
            signals=[sig], confirmed=[sig],
            corrections=[{"model": "gpt-4", "action": CorrectionAction.ESCALATE_HUMAN.value}],
            created_at=datetime.now(timezone.utc),
        )

        with (
            patch.object(DriftDetector, "detect", return_value=[sig]),
            patch.object(DriftCorrector, "confirm_and_correct", return_value=report),
            patch("core.evaluation.drift_pipeline._try_build_regression_gate", return_value=None),
            patch("core.evaluation.drift_pipeline._try_build_prompt_optimizer", return_value=None),
        ):
            result = run_drift_pipeline(db_factory)

        assert result.corrections_applied == 0

    def test_none_action_not_counted(self):
        db = MagicMock()
        db_factory = MagicMock(return_value=db)
        sig = _signal(severity=DriftSeverity.MILD)

        report = DriftReport(
            signals=[sig], confirmed=[],
            corrections=[{"model": "gpt-4", "action": CorrectionAction.NONE.value}],
            created_at=datetime.now(timezone.utc),
        )

        with (
            patch.object(DriftDetector, "detect", return_value=[sig]),
            patch.object(DriftCorrector, "confirm_and_correct", return_value=report),
            patch("core.evaluation.drift_pipeline._try_build_regression_gate", return_value=None),
            patch("core.evaluation.drift_pipeline._try_build_prompt_optimizer", return_value=None),
        ):
            result = run_drift_pipeline(db_factory)

        assert result.signals_detected == 1
        assert result.signals_confirmed == 0
        assert result.corrections_applied == 0

    def test_multiple_signals_mixed_actions(self):
        db = MagicMock()
        db_factory = MagicMock(return_value=db)
        sig1 = _signal(model="gpt-4", severity=DriftSeverity.SIGNIFICANT)
        sig2 = _signal(model="claude-3", severity=DriftSeverity.SEVERE)

        report = DriftReport(
            signals=[sig1, sig2], confirmed=[sig1, sig2],
            corrections=[
                {"model": "gpt-4", "action": CorrectionAction.OPTIMIZE_PROMPT.value},
                {"model": "claude-3", "action": CorrectionAction.FALLBACK_MODEL.value},
            ],
            created_at=datetime.now(timezone.utc),
        )

        with (
            patch.object(DriftDetector, "detect", return_value=[sig1, sig2]),
            patch.object(DriftCorrector, "confirm_and_correct", return_value=report),
            patch("core.evaluation.drift_pipeline._try_build_regression_gate", return_value=None),
            patch("core.evaluation.drift_pipeline._try_build_prompt_optimizer", return_value=None),
        ):
            result = run_drift_pipeline(db_factory)

        assert result.signals_detected == 2
        assert result.corrections_applied == 2


class TestPipelineErrorHandling:
    def test_detect_exception_returns_error(self):
        db = MagicMock()
        db_factory = MagicMock(return_value=db)

        with patch.object(DriftDetector, "detect", side_effect=RuntimeError("DB down")):
            result = run_drift_pipeline(db_factory)

        assert result.error == "DB down"
        assert result.signals_detected == 0
        db.close.assert_called_once()

    def test_db_close_failure_non_fatal(self):
        db = MagicMock()
        db.close.side_effect = RuntimeError("close failed")
        db_factory = MagicMock(return_value=db)

        with patch.object(DriftDetector, "detect", return_value=[]):
            result = run_drift_pipeline(db_factory)

        assert result.error is None  # pipeline succeeded despite close failure


class TestPipelineAsync:
    def test_async_starts_thread(self):
        from core.evaluation.drift_pipeline import run_drift_pipeline_async, _pipeline_lock

        db_factory = MagicMock()
        # Ensure lock is free
        if _pipeline_lock.locked():
            _pipeline_lock.release()

        with patch("core.evaluation.drift_pipeline.run_drift_pipeline"):
            with patch("core.evaluation.drift_pipeline.threading") as mock_threading:
                # Mock lock to avoid real threading
                with patch("core.evaluation.drift_pipeline._pipeline_lock") as mock_lock:
                    mock_lock.acquire.return_value = True
                    run_drift_pipeline_async(db_factory)
                    mock_threading.Thread.assert_called_once()
                    mock_threading.Thread.return_value.start.assert_called_once()

    def test_async_skips_if_already_running(self):
        from core.evaluation.drift_pipeline import run_drift_pipeline_async

        db_factory = MagicMock()
        with patch("core.evaluation.drift_pipeline._pipeline_lock") as mock_lock:
            mock_lock.acquire.return_value = False
            with patch("core.evaluation.drift_pipeline.threading") as mock_threading:
                run_drift_pipeline_async(db_factory)
                mock_threading.Thread.assert_not_called()
