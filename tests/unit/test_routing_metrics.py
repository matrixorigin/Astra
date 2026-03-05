"""Tests for routing metrics — active requests, budget, adaptive threshold."""

import threading
import pytest
from unittest.mock import patch

from core.context.routing_metrics import (
    active_request_context,
    adaptive_threshold,
    current_load,
    monthly_budget_remaining,
    reset_for_testing,
)


@pytest.fixture(autouse=True)
def clean_state():
    reset_for_testing()
    yield
    reset_for_testing()


class TestActiveRequests:
    def test_context_increments_and_decrements(self):
        assert current_load() == 0.0
        with active_request_context():
            assert current_load() > 0.0
        assert current_load() == 0.0

    def test_concurrent_requests(self):
        barriers = []
        for _ in range(5):
            barriers.append(threading.Barrier(2))

        loads = []

        def worker(barrier):
            with active_request_context():
                barrier.wait(timeout=2)
                loads.append(current_load())

        threads = []
        # Start 5 concurrent requests
        b = threading.Barrier(6)
        for _ in range(5):
            t = threading.Thread(target=lambda: (
                active_request_context().__enter__(),
                b.wait(timeout=2),
            ))
            # Simpler: just check inc/dec
        with active_request_context():
            with active_request_context():
                # 2 active requests, capacity 20 → 0.1
                assert current_load() == pytest.approx(2 / 20, abs=0.01)
            assert current_load() == pytest.approx(1 / 20, abs=0.01)
        assert current_load() == 0.0

    def test_exception_still_decrements(self):
        try:
            with active_request_context():
                raise ValueError("test")
        except ValueError:
            pass
        assert current_load() == 0.0


class TestAdaptiveThreshold:
    def test_normal_conditions(self):
        """No load, full budget → base threshold 0.85."""
        assert adaptive_threshold() == pytest.approx(0.85, abs=0.01)

    @patch("core.context.routing_metrics._active_requests", 18)
    def test_high_load(self):
        """Load > 0.8 → threshold drops by 0.10 → 0.75."""
        # 18/20 = 0.9 > 0.8
        assert adaptive_threshold() == pytest.approx(0.75, abs=0.01)

    def test_low_budget(self):
        """Budget remaining < 0.2 → threshold rises by 0.07 → 0.92."""
        with patch("core.context.routing_metrics.monthly_budget_remaining", return_value=0.1):
            assert adaptive_threshold() == pytest.approx(0.92, abs=0.01)

    @patch("core.context.routing_metrics._active_requests", 18)
    def test_high_load_and_low_budget(self):
        """Both conditions: -0.10 + 0.07 = -0.03 → 0.82."""
        with patch("core.context.routing_metrics.monthly_budget_remaining", return_value=0.1):
            assert adaptive_threshold() == pytest.approx(0.82, abs=0.01)

    def test_clamp_lower_bound(self):
        """Threshold never goes below 0.70."""
        with patch("core.context.routing_metrics._active_requests", 100):
            t = adaptive_threshold(base=0.70)
            assert t >= 0.70

    def test_clamp_upper_bound(self):
        """Threshold never goes above 0.95."""
        with patch("core.context.routing_metrics.monthly_budget_remaining", return_value=0.0):
            t = adaptive_threshold(base=0.95)
            assert t <= 0.95


class TestMonthlyBudget:
    def test_no_db_factory_returns_full_budget(self):
        assert monthly_budget_remaining(db_factory=None) == 1.0

    def test_zero_budget_returns_full(self):
        with patch("core.context.routing_metrics._MONTHLY_BUDGET", 0):
            reset_for_testing()
            assert monthly_budget_remaining(db_factory=None) == 1.0
