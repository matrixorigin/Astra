"""Tests for ConfidenceCalibrator."""

from unittest.mock import Mock

import pytest

from core.evaluation.confidence_calibrator import ConfidenceCalibrator


def _calibrator(pairs: list[tuple[float, float]]) -> ConfidenceCalibrator:
    """Build calibrator with mocked DB returning (confidence, quality) pairs."""
    db = Mock()
    db.execute.return_value.fetchall.return_value = pairs
    return ConfidenceCalibrator(lambda: db)


class TestMeasure:
    def test_no_data_returns_zero_error(self):
        cal = _calibrator([])
        result = cal.measure()
        assert result.sample_count == 0
        assert result.calibration_error == 0.0
        assert result.bias == 0.0

    def test_perfect_calibration(self):
        # confidence == quality/5 for all samples
        pairs = [(0.8, 4.0), (0.6, 3.0), (1.0, 5.0)]
        cal = _calibrator(pairs)
        result = cal.measure()
        assert result.bias == pytest.approx(0.0, abs=0.01)
        assert result.calibration_error == pytest.approx(0.0, abs=0.01)

    def test_overconfident_positive_bias(self):
        # confidence always higher than quality/5
        pairs = [(0.9, 2.0), (0.9, 2.0), (0.9, 2.0)]  # conf=0.9, qual=0.4
        cal = _calibrator(pairs)
        result = cal.measure()
        assert result.bias > 0  # overconfident

    def test_underconfident_negative_bias(self):
        # confidence always lower than quality/5
        pairs = [(0.2, 5.0), (0.2, 5.0), (0.2, 5.0)]  # conf=0.2, qual=1.0
        cal = _calibrator(pairs)
        result = cal.measure()
        assert result.bias < 0  # underconfident

    def test_sample_count_correct(self):
        pairs = [(0.8, 4.0)] * 10
        cal = _calibrator(pairs)
        result = cal.measure()
        assert result.sample_count == 10

    def test_bucket_errors_length(self):
        pairs = [(0.8, 4.0)] * 5
        cal = _calibrator(pairs)
        result = cal.measure()
        assert len(result.bucket_errors) == cal.BUCKETS


class TestComputeAdjustment:
    def test_insufficient_data_returns_1(self):
        cal = _calibrator([])
        from core.evaluation.confidence_calibrator import CalibrationResult

        result = CalibrationResult(
            mean_confidence=0.8,
            mean_quality=0.4,
            calibration_error=0.4,
            bias=0.4,
            sample_count=5,
            bucket_errors=[],
        )
        adj = cal.compute_adjustment(result)
        assert adj["multiplier"] == 1.0
        assert adj["reason"] == "insufficient_data"

    def test_well_calibrated_returns_1(self):
        cal = _calibrator([])
        from core.evaluation.confidence_calibrator import CalibrationResult

        result = CalibrationResult(
            mean_confidence=0.8,
            mean_quality=0.78,
            calibration_error=0.02,
            bias=0.02,
            sample_count=100,
            bucket_errors=[],
        )
        adj = cal.compute_adjustment(result)
        assert adj["multiplier"] == 1.0
        assert adj["reason"] == "well_calibrated"

    def test_overconfident_multiplier_below_1(self):
        cal = _calibrator([])
        from core.evaluation.confidence_calibrator import CalibrationResult

        result = CalibrationResult(
            mean_confidence=0.9,
            mean_quality=0.5,
            calibration_error=0.4,
            bias=0.4,
            sample_count=100,
            bucket_errors=[],
        )
        adj = cal.compute_adjustment(result)
        assert adj["multiplier"] < 1.0
        assert adj["reason"] == "overconfident"

    def test_underconfident_multiplier_above_1(self):
        cal = _calibrator([])
        from core.evaluation.confidence_calibrator import CalibrationResult

        result = CalibrationResult(
            mean_confidence=0.3,
            mean_quality=0.8,
            calibration_error=0.5,
            bias=-0.5,
            sample_count=100,
            bucket_errors=[],
        )
        adj = cal.compute_adjustment(result)
        assert adj["multiplier"] > 1.0
        assert adj["reason"] == "underconfident"

    def test_multiplier_clamped_to_range(self):
        cal = _calibrator([])
        from core.evaluation.confidence_calibrator import CalibrationResult

        # Extreme overconfidence
        result = CalibrationResult(
            mean_confidence=1.0,
            mean_quality=0.0,
            calibration_error=1.0,
            bias=1.0,
            sample_count=100,
            bucket_errors=[],
        )
        adj = cal.compute_adjustment(result)
        assert adj["multiplier"] >= 0.5  # clamped at 0.5


class TestECEBuckets:
    def test_ece_buckets_sum_to_total_error(self):
        pairs = [(0.1, 1.0), (0.5, 2.5), (0.9, 4.5)]
        cal = _calibrator(pairs)
        result = cal.measure()
        total_weighted = sum(b["weighted_error"] for b in result.bucket_errors)
        assert total_weighted == pytest.approx(result.calibration_error, abs=0.001)

    def test_empty_bucket_has_zero_error(self):
        # All samples in [0.8, 1.0) bucket
        pairs = [(0.85, 4.0)] * 5
        cal = _calibrator(pairs)
        result = cal.measure()
        # First bucket [0.0, 0.2) should be empty
        first_bucket = result.bucket_errors[0]
        assert first_bucket["count"] == 0
        assert first_bucket["weighted_error"] == 0
