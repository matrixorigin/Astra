"""Tests for SLO Monitor and burn-rate alerting."""

from unittest.mock import MagicMock, Mock, patch

import pytest

from core.evaluation.slo_monitor import (
    DEFAULT_SLOS,
    SLOMonitor,
    SLOSeverity,
    SLOStatus,
    SLOTarget,
)


def _monitor(daily_rows: list[tuple]) -> SLOMonitor:
    db = Mock()
    db.execute.return_value.fetchall.return_value = daily_rows
    return SLOMonitor(db=db)


def _daily(avg_quality: float, hallucination_rate: float = 0.0, n: int = 10):
    """Build a fake daily metrics row: (day, avg_quality, hallucination_rate, total)."""
    from datetime import date
    return (date.today(), avg_quality, hallucination_rate, n)


class TestSLOEvaluation:
    def test_quality_slo_met(self):
        monitor = _monitor([_daily(4.5)] * 10)
        report = monitor.check_agent("agent-1", period_days=10)
        quality = next(s for s in report.statuses if s.slo.name == "quality")
        assert quality.met is True
        assert quality.severity == SLOSeverity.OK

    def test_quality_slo_not_met(self):
        monitor = _monitor([_daily(3.0)] * 30)
        report = monitor.check_agent("agent-1", period_days=30)
        quality = next(s for s in report.statuses if s.slo.name == "quality")
        assert quality.met is False
        assert quality.severity == SLOSeverity.BREACH

    def test_hallucination_slo_met(self):
        monitor = _monitor([_daily(4.5, hallucination_rate=0.01)] * 10)
        report = monitor.check_agent("agent-1")
        hall = next(s for s in report.statuses if s.slo.name == "hallucination_rate")
        assert hall.met is True

    def test_hallucination_slo_violated(self):
        monitor = _monitor([_daily(4.5, hallucination_rate=0.05)] * 30)
        report = monitor.check_agent("agent-1", period_days=30)
        hall = next(s for s in report.statuses if s.slo.name == "hallucination_rate")
        assert hall.met is False

    def test_no_data_returns_ok(self):
        monitor = _monitor([])
        report = monitor.check_agent("agent-1")
        for s in report.statuses:
            assert s.severity == SLOSeverity.OK
            assert s.days_elapsed == 0


class TestBurnRate:
    def test_burn_rate_zero_when_all_good(self):
        monitor = _monitor([_daily(4.5)] * 15)
        report = monitor.check_agent("agent-1", period_days=15)
        quality = next(s for s in report.statuses if s.slo.name == "quality")
        assert quality.burn_rate == 0.0

    def test_burn_rate_warning_threshold(self):
        # 50% bad days → projected 15 bad days / 1.5 allowed = 10x burn rate → CRITICAL
        monitor = _monitor([_daily(3.0)] * 15 + [_daily(4.5)] * 15)
        report = monitor.check_agent("agent-1", period_days=30)
        quality = next(s for s in report.statuses if s.slo.name == "quality")
        assert quality.burn_rate > 1.5

    def test_severity_warning_at_1_5x(self):
        slo = SLOTarget("quality", "avg_quality", 4.0, ">=")
        monitor = SLOMonitor(db=Mock(), slos=[slo])
        # 3 bad days out of 30 → projected 3 bad days / 1.5 allowed = 2x → WARNING
        status = monitor._evaluate_slo(
            slo,
            [{"avg_quality": 3.0, "hallucination_rate": 0.0, "completion_rate": 0.95}] * 3
            + [{"avg_quality": 4.5, "hallucination_rate": 0.0, "completion_rate": 0.95}] * 27,
            period_days=30,
        )
        assert status.severity == SLOSeverity.WARNING

    def test_severity_critical_at_3x(self):
        slo = SLOTarget("quality", "avg_quality", 4.0, ">=")
        monitor = SLOMonitor(db=Mock(), slos=[slo])
        # 5 bad days out of 30 → projected 5 / 1.5 = 3.3x → CRITICAL
        status = monitor._evaluate_slo(
            slo,
            [{"avg_quality": 3.0, "hallucination_rate": 0.0, "completion_rate": 0.95}] * 5
            + [{"avg_quality": 4.5, "hallucination_rate": 0.0, "completion_rate": 0.95}] * 25,
            period_days=30,
        )
        assert status.severity == SLOSeverity.CRITICAL

    def test_breach_when_period_complete_and_not_met(self):
        slo = SLOTarget("quality", "avg_quality", 4.0, ">=")
        monitor = SLOMonitor(db=Mock(), slos=[slo])
        status = monitor._evaluate_slo(
            slo,
            [{"avg_quality": 3.0, "hallucination_rate": 0.0, "completion_rate": 0.95}] * 30,
            period_days=30,
        )
        assert status.severity == SLOSeverity.BREACH


class TestSLOClassifySeverity:
    def test_ok(self):
        assert SLOMonitor._classify_severity(0.5, True, 10, 30) == SLOSeverity.OK

    def test_warning(self):
        assert SLOMonitor._classify_severity(2.0, True, 10, 30) == SLOSeverity.WARNING

    def test_critical(self):
        assert SLOMonitor._classify_severity(4.0, True, 10, 30) == SLOSeverity.CRITICAL

    def test_breach_overrides_burn_rate(self):
        # Even if burn rate is low, breach if period complete and not met
        assert SLOMonitor._classify_severity(0.1, False, 30, 30) == SLOSeverity.BREACH
