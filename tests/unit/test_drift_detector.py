"""Tests for DriftDetector and DriftCorrector."""

from datetime import datetime, timezone
from unittest.mock import Mock, patch

import pytest

from core.evaluation.drift_detector import (
    CorrectionAction,
    DriftCorrector,
    DriftDetector,
    DriftSeverity,
    DriftSignal,
)


def _signal(
    model: str = "gpt-4",
    template_id: str | None = None,
    current_avg: float = 3.0,
    previous_avg: float = 4.0,
    sample_count: int = 10,
) -> DriftSignal:
    delta = current_avg - previous_avg
    severity = DriftDetector._classify(delta, sample_count)
    return DriftSignal(
        model=model, template_id=template_id,
        current_avg=current_avg, previous_avg=previous_avg,
        week_delta=delta, severity=severity,
        sample_count=sample_count,
        detected_at=datetime.now(timezone.utc),
    )


class TestDriftClassification:
    def test_no_drift(self):
        assert DriftDetector._classify(-0.1, 10) == DriftSeverity.NONE

    def test_mild_drift(self):
        assert DriftDetector._classify(-0.4, 10) == DriftSeverity.MILD

    def test_significant_drift(self):
        assert DriftDetector._classify(-0.7, 10) == DriftSeverity.SIGNIFICANT

    def test_severe_drift(self):
        assert DriftDetector._classify(-1.5, 10) == DriftSeverity.SEVERE

    def test_insufficient_samples_returns_none(self):
        assert DriftDetector._classify(-2.0, 3) == DriftSeverity.NONE

    def test_exactly_at_mild_threshold(self):
        # -0.3 is the boundary: < -0.3 triggers MILD, so -0.3 itself is NONE
        assert DriftDetector._classify(-0.3, 10) == DriftSeverity.NONE
        assert DriftDetector._classify(-0.31, 10) == DriftSeverity.MILD

    def test_exactly_at_significant_threshold(self):
        # -0.5 is the boundary: < -0.5 triggers SIGNIFICANT, so -0.5 itself is MILD
        assert DriftDetector._classify(-0.5, 10) == DriftSeverity.MILD
        assert DriftDetector._classify(-0.51, 10) == DriftSeverity.SIGNIFICANT

    def test_positive_delta_no_drift(self):
        assert DriftDetector._classify(0.5, 10) == DriftSeverity.NONE


class TestDriftDetectorQuery:
    def test_detect_returns_only_non_none_signals(self):
        db = Mock()
        # Model drift: one row with significant drop
        db.execute.return_value.fetchall.side_effect = [
            [(  "gpt-4", 3.0, 4.0, 10)],  # model drift
            [],  # template drift
        ]
        detector = DriftDetector(db=db)
        signals = detector.detect()
        assert len(signals) == 1
        assert signals[0].model == "gpt-4"
        assert signals[0].severity == DriftSeverity.SIGNIFICANT

    def test_detect_filters_none_severity(self):
        db = Mock()
        db.execute.return_value.fetchall.side_effect = [
            [("gpt-4", 4.0, 4.1, 10)],  # tiny drop → NONE
            [],
        ]
        detector = DriftDetector(db=db)
        signals = detector.detect()
        assert signals == []

    def test_detect_skips_null_averages(self):
        db = Mock()
        db.execute.return_value.fetchall.side_effect = [
            [("gpt-4", None, 4.0, 10)],  # recent_avg is None
            [],
        ]
        detector = DriftDetector(db=db)
        signals = detector.detect()
        assert signals == []

    def test_build_signal_correct_delta(self):
        db = Mock()
        detector = DriftDetector(db=db)
        sig = detector._build_signal(
            model="gpt-4", template_id=None,
            recent_avg=3.2, previous_avg=4.0, sample_count=10,
        )
        assert sig.week_delta == pytest.approx(-0.8, abs=0.001)
        assert sig.severity == DriftSeverity.SIGNIFICANT


class TestDriftCorrector:
    def test_no_gate_confirms_significant_drift(self):
        corrector = DriftCorrector(db=Mock(), regression_gate=None, router=None)
        sig = _signal(current_avg=3.0, previous_avg=4.0)  # SIGNIFICANT
        assert corrector._confirm(sig) is True

    def test_no_gate_does_not_confirm_mild_drift(self):
        corrector = DriftCorrector(db=Mock(), regression_gate=None, router=None)
        sig = _signal(current_avg=3.8, previous_avg=4.0)  # MILD
        assert corrector._confirm(sig) is False

    def test_gate_fail_confirms_drift(self):
        gate = Mock()
        gate.validate_change.return_value = {"verdict": "fail"}
        corrector = DriftCorrector(db=Mock(), regression_gate=gate, router=None)
        sig = _signal()
        assert corrector._confirm(sig) is True

    def test_gate_pass_does_not_confirm(self):
        gate = Mock()
        gate.validate_change.return_value = {"verdict": "pass"}
        corrector = DriftCorrector(db=Mock(), regression_gate=gate, router=None)
        sig = _signal()
        assert corrector._confirm(sig) is False

    def test_gate_exception_falls_back_to_severity(self):
        gate = Mock()
        gate.validate_change.side_effect = RuntimeError("gate down")
        corrector = DriftCorrector(db=Mock(), regression_gate=gate, router=None)
        sig = _signal(current_avg=3.0, previous_avg=4.0)  # SIGNIFICANT
        assert corrector._confirm(sig) is True

    def test_fallback_applied_when_router_has_fallback(self):
        router = Mock()
        router.get.return_value = Mock(fallback_to="gpt-3.5", is_active=True)
        db = Mock()
        db.execute.return_value = Mock()
        corrector = DriftCorrector(db=db, regression_gate=None, router=router)
        sig = _signal(current_avg=3.0, previous_avg=4.0)  # SIGNIFICANT
        action = corrector._apply_fallback(sig)
        assert action == CorrectionAction.FALLBACK_MODEL

    def test_escalate_when_no_fallback(self):
        router = Mock()
        router.get.return_value = Mock(fallback_to=None)
        corrector = DriftCorrector(db=Mock(), regression_gate=None, router=router)
        sig = _signal()
        action = corrector._apply_fallback(sig)
        assert action == CorrectionAction.ESCALATE_HUMAN

    def test_escalate_when_no_router(self):
        corrector = DriftCorrector(db=Mock(), regression_gate=None, router=None)
        sig = _signal()
        action = corrector._apply_fallback(sig)
        assert action == CorrectionAction.ESCALATE_HUMAN

    def test_confirm_and_correct_returns_report(self):
        db = Mock()
        db.execute.return_value = Mock()
        corrector = DriftCorrector(db=db, regression_gate=None, router=None)
        signals = [_signal(current_avg=3.0, previous_avg=4.0)]  # SIGNIFICANT → confirmed
        report = corrector.confirm_and_correct(signals)
        assert len(report.signals) == 1
        assert len(report.confirmed) == 1
        assert len(report.corrections) == 1

    def test_mild_signal_not_confirmed(self):
        db = Mock()
        corrector = DriftCorrector(db=db, regression_gate=None, router=None)
        signals = [_signal(current_avg=3.8, previous_avg=4.0)]  # MILD → not confirmed
        report = corrector.confirm_and_correct(signals)
        assert len(report.confirmed) == 0
        assert len(report.corrections) == 0

    def test_template_drift_triggers_prompt_optimization(self):
        """Template-level drift should try prompt optimization before fallback."""
        optimizer = Mock()
        optimizer.optimize.return_value = Mock(activated=True, old_version="1.0", new_version="1.1")
        db = Mock()
        db.execute.return_value = Mock()
        corrector = DriftCorrector(db=db, prompt_optimizer=optimizer)
        sig = _signal(current_avg=3.0, previous_avg=4.0, template_id="system_general")
        correction = corrector._correct(sig)
        assert correction["action"] == CorrectionAction.OPTIMIZE_PROMPT.value
        optimizer.optimize.assert_called_once_with(template_id="system_general", min_cases=2)

    def test_prompt_optimization_failure_falls_through_to_fallback(self):
        """If prompt optimization fails, should try model fallback."""
        optimizer = Mock()
        optimizer.optimize.return_value = Mock(activated=False)
        router = Mock()
        router.get.return_value = Mock(fallback_to="gpt-3.5", is_active=True)
        db = Mock()
        db.execute.return_value = Mock()
        corrector = DriftCorrector(db=db, prompt_optimizer=optimizer, router=router)
        sig = _signal(current_avg=3.0, previous_avg=4.0, template_id="system_general")
        correction = corrector._correct(sig)
        assert correction["action"] == CorrectionAction.FALLBACK_MODEL.value

    def test_no_template_id_skips_prompt_optimization(self):
        """Model-level drift (no template_id) should not try prompt optimization."""
        optimizer = Mock()
        router = Mock()
        router.get.return_value = Mock(fallback_to="gpt-3.5", is_active=True)
        db = Mock()
        db.execute.return_value = Mock()
        corrector = DriftCorrector(db=db, prompt_optimizer=optimizer, router=router)
        sig = _signal(current_avg=3.0, previous_avg=4.0, template_id=None)
        correction = corrector._correct(sig)
        optimizer.optimize.assert_not_called()
        assert correction["action"] == CorrectionAction.FALLBACK_MODEL.value

    def test_optimize_prompt_in_correction_action_enum(self):
        assert CorrectionAction.OPTIMIZE_PROMPT.value == "optimize_prompt"
