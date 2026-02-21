"""Tests for RetrainingMonitor and ABTestRouter."""

from datetime import datetime, timezone, timedelta
from unittest.mock import MagicMock

from core.models.retraining_monitor import RetrainingMonitor, _GROWTH_THRESHOLD, _STALENESS_DAYS
from core.models.ab_test import ABTestRouter, ABTestConfig, _hash_bucket


# ---------------------------------------------------------------------------
# RetrainingMonitor
# ---------------------------------------------------------------------------

class TestRetrainingMonitor:

    def _make(self, feedback_count=0, artifact=None):
        db = MagicMock()
        monitor = RetrainingMonitor(db)
        monitor._feedback_count = MagicMock(return_value=feedback_count)
        monitor._last_artifact = MagicMock(return_value=artifact)
        return monitor

    def test_no_model_sufficient_data(self):
        m = self._make(feedback_count=100, artifact=None)
        r = m.should_retrain()
        assert r["needed"] is True
        assert r["reason"] == "no_model_exists"

    def test_no_model_insufficient_data(self):
        m = self._make(feedback_count=10, artifact=None)
        r = m.should_retrain()
        assert r["needed"] is False
        assert r["reason"] == "insufficient_data"

    def test_data_growth_triggers(self):
        """20% growth should trigger retrain."""
        artifact = {"artifact_id": "a", "dataset_size": 1000,
                    "created_at": datetime.now(timezone.utc) - timedelta(days=5)}
        m = self._make(feedback_count=1200, artifact=artifact)
        r = m.should_retrain()
        assert r["needed"] is True
        assert r["reason"] == "data_growth"

    def test_no_growth_no_retrain(self):
        artifact = {"artifact_id": "a", "dataset_size": 1000,
                    "created_at": datetime.now(timezone.utc) - timedelta(days=5)}
        m = self._make(feedback_count=1100, artifact=artifact)
        r = m.should_retrain()
        assert r["needed"] is False

    def test_stale_model_triggers(self):
        artifact = {"artifact_id": "a", "dataset_size": 1000,
                    "created_at": datetime.now(timezone.utc) - timedelta(days=_STALENESS_DAYS + 1)}
        m = self._make(feedback_count=1050, artifact=artifact)
        r = m.should_retrain()
        assert r["needed"] is True
        assert r["reason"] == "stale_model"

    def test_fresh_model_no_retrain(self):
        artifact = {"artifact_id": "a", "dataset_size": 1000,
                    "created_at": datetime.now(timezone.utc) - timedelta(days=1)}
        m = self._make(feedback_count=1050, artifact=artifact)
        r = m.should_retrain()
        assert r["needed"] is False
        assert r["reason"] == "up_to_date"

    def test_zero_dataset_size_no_division_error(self):
        artifact = {"artifact_id": "a", "dataset_size": 0,
                    "created_at": datetime.now(timezone.utc)}
        m = self._make(feedback_count=100, artifact=artifact)
        # Should not raise ZeroDivisionError
        r = m.should_retrain()
        assert isinstance(r["needed"], bool)


# ---------------------------------------------------------------------------
# ABTestRouter
# ---------------------------------------------------------------------------

class TestABTestRouter:

    def test_no_experiment_returns_none(self):
        router = ABTestRouter()
        assert router.route("nonexistent", "session-1") is None

    def test_register_and_route(self):
        router = ABTestRouter()
        config = ABTestConfig(
            experiment_name="fc_v2",
            control_artifact_id="aid-1",
            treatment_artifact_id="aid-2",
            treatment_pct=50,
        )
        router.register(config)
        result = router.route("fc_v2", "session-1")
        assert result is not None
        assert result.group in ("control", "treatment")
        assert result.artifact_id in ("aid-1", "aid-2")

    def test_deterministic_routing(self):
        """Same session always gets same group."""
        router = ABTestRouter()
        config = ABTestConfig(
            experiment_name="fc_v2",
            control_artifact_id="aid-1",
            treatment_artifact_id="aid-2",
            treatment_pct=50,
        )
        router.register(config)
        results = [router.route("fc_v2", "session-abc").group for _ in range(10)]
        assert len(set(results)) == 1  # All same

    def test_different_sessions_get_different_groups(self):
        """With 50% split, different sessions should eventually get both groups."""
        router = ABTestRouter()
        config = ABTestConfig(
            experiment_name="fc_v2",
            control_artifact_id="aid-1",
            treatment_artifact_id="aid-2",
            treatment_pct=50,
        )
        router.register(config)
        groups = {router.route("fc_v2", f"session-{i}").group for i in range(100)}
        assert groups == {"control", "treatment"}

    def test_zero_pct_all_control(self):
        router = ABTestRouter()
        config = ABTestConfig(
            experiment_name="fc_v2",
            control_artifact_id="aid-1",
            treatment_artifact_id="aid-2",
            treatment_pct=0,
        )
        router.register(config)
        groups = {router.route("fc_v2", f"s-{i}").group for i in range(50)}
        assert groups == {"control"}

    def test_100_pct_all_treatment(self):
        router = ABTestRouter()
        config = ABTestConfig(
            experiment_name="fc_v2",
            control_artifact_id="aid-1",
            treatment_artifact_id="aid-2",
            treatment_pct=100,
        )
        router.register(config)
        groups = {router.route("fc_v2", f"s-{i}").group for i in range(50)}
        assert groups == {"treatment"}

    def test_remove_experiment(self):
        router = ABTestRouter()
        config = ABTestConfig(
            experiment_name="fc_v2",
            control_artifact_id="aid-1",
            treatment_artifact_id="aid-2",
        )
        router.register(config)
        assert router.remove("fc_v2") is True
        assert router.route("fc_v2", "s-1") is None
        assert router.remove("fc_v2") is False

    def test_list_experiments(self):
        router = ABTestRouter()
        assert router.list_experiments() == []
        router.register(ABTestConfig("exp1", "a", "b"))
        assert router.list_experiments() == ["exp1"]


class TestHashBucket:

    def test_range_0_99(self):
        for i in range(200):
            b = _hash_bucket(f"session-{i}", "salt")
            assert 0 <= b <= 99

    def test_deterministic(self):
        assert _hash_bucket("s1", "salt") == _hash_bucket("s1", "salt")

    def test_different_salt_different_bucket(self):
        buckets = {_hash_bucket("s1", f"salt-{i}") for i in range(20)}
        assert len(buckets) > 1


class TestABTestConfigValidation:

    def test_negative_pct_clamped(self):
        c = ABTestConfig("exp", "a", "b", treatment_pct=-10)
        assert c.treatment_pct == 0

    def test_over_100_clamped(self):
        c = ABTestConfig("exp", "a", "b", treatment_pct=200)
        assert c.treatment_pct == 100
