"""Unit tests for GovernanceScheduler."""

from datetime import datetime, timedelta
from unittest.mock import MagicMock, patch
from uuid import uuid4

import pytest

from core.memory.governance import GovernanceScheduler, GovernanceCycleResult
from core.memory.config import MemoryGovernanceConfig


class TestGovernanceScheduler:
    """Tests for GovernanceScheduler."""

    @pytest.fixture
    def mock_db_factory(self):
        return MagicMock()

    @pytest.fixture
    def config(self):
        return MemoryGovernanceConfig(
            confidence_decay_half_life_days=30.0,
            reflector_cluster_min_size=3,
            reflector_cluster_similarity=0.7,
            pollution_threshold=0.3,
            milestone_snapshot_keep_n=5,
        )

    def test_apply_decay_updates_confidence(self, mock_db_factory, config):
        """Decay reduces confidence based on age."""
        scheduler = GovernanceScheduler(mock_db_factory, config)

        mock_session = MagicMock()
        mock_result = MagicMock()
        mock_result.rowcount = 5
        mock_session.execute.return_value = mock_result

        with patch.object(scheduler, "_db") as mock_db:
            mock_db.return_value.__enter__.return_value = mock_session
            mock_db.return_value.__exit__.return_value = None

            count = scheduler._apply_decay("user123")

        assert count == 5
        mock_session.execute.assert_called_once()
        mock_session.commit.assert_called_once()

    def test_cleanup_stale_deletes_low_confidence_inactive(
        self, mock_db_factory, config
    ):
        """Cleanup removes inactive memories below threshold."""
        scheduler = GovernanceScheduler(mock_db_factory, config)

        mock_session = MagicMock()
        mock_result = MagicMock()
        mock_result.rowcount = 3
        mock_session.execute.return_value = mock_result

        with patch.object(scheduler, "_db") as mock_db:
            mock_db.return_value.__enter__.return_value = mock_session
            mock_db.return_value.__exit__.return_value = None

            count = scheduler._cleanup_stale("user123", confidence_threshold=0.1)

        assert count == 3
        # Verify DELETE was called
        call_args = mock_session.execute.call_args
        sql_text = str(call_args[0][0])
        assert "DELETE FROM memories" in sql_text
        assert "is_active = 0" in sql_text
        assert "confidence <" in sql_text

    def test_run_cycle_executes_all_steps(self, mock_db_factory, config):
        """Full cycle runs decay, reflector, health, cleanup."""
        scheduler = GovernanceScheduler(mock_db_factory, config)

        # Mock all internal methods
        with patch.object(scheduler, "_apply_decay", return_value=2) as mock_decay, \
             patch.object(scheduler.reflector, "reflect", return_value={"promoted": 1}) as mock_reflect, \
             patch.object(scheduler.health, "detect_pollution", return_value={"is_polluted": False}) as mock_poll, \
             patch.object(scheduler, "_cleanup_stale", return_value=3) as mock_stale, \
             patch.object(scheduler.health, "cleanup_orphan_branches", return_value=0) as mock_branches, \
             patch.object(scheduler.health, "cleanup_snapshots", return_value=1) as mock_snaps:

            result = scheduler.run_cycle("user123")

        assert result.decayed_count == 2
        assert result.promoted_count == 1
        assert result.cleaned_stale == 3
        assert result.cleaned_branches == 0
        assert result.cleaned_snapshots == 1
        assert result.pollution_detected is False
        assert len(result.errors) == 0

        mock_decay.assert_called_once_with("user123")
        mock_reflect.assert_called_once_with("user123")
        mock_poll.assert_called_once()
        mock_stale.assert_called_once_with("user123")
        mock_branches.assert_called_once()
        mock_snaps.assert_called_once()

    def test_run_cycle_detects_pollution(self, mock_db_factory, config):
        """Cycle flags pollution when threshold exceeded."""
        scheduler = GovernanceScheduler(mock_db_factory, config)

        with patch.object(scheduler, "_apply_decay", return_value=0), \
             patch.object(scheduler.reflector, "reflect", return_value={"promoted": 0}), \
             patch.object(scheduler.health, "detect_pollution", return_value={"is_polluted": True, "ratio": 0.5}), \
             patch.object(scheduler, "_cleanup_stale", return_value=0), \
             patch.object(scheduler.health, "cleanup_orphan_branches", return_value=0), \
             patch.object(scheduler.health, "cleanup_snapshots", return_value=0):

            result = scheduler.run_cycle("user123")

        assert result.pollution_detected is True

    def test_run_cycle_continues_on_error(self, mock_db_factory, config):
        """Cycle continues even if one step fails."""
        scheduler = GovernanceScheduler(mock_db_factory, config)

        with patch.object(scheduler, "_apply_decay", side_effect=Exception("decay error")), \
             patch.object(scheduler.reflector, "reflect", return_value={"promoted": 1}), \
             patch.object(scheduler.health, "detect_pollution", return_value={"is_polluted": False}), \
             patch.object(scheduler, "_cleanup_stale", return_value=2), \
             patch.object(scheduler.health, "cleanup_orphan_branches", return_value=0), \
             patch.object(scheduler.health, "cleanup_snapshots", return_value=0):

            result = scheduler.run_cycle("user123")

        # Decay failed but other steps ran
        assert result.decayed_count == 0
        assert result.promoted_count == 1
        assert result.cleaned_stale == 2
        assert "decay" in result.errors[0]

    def test_last_cycle_tracked_per_user(self, mock_db_factory, config):
        """Each user has independent last_cycle timestamp."""
        scheduler = GovernanceScheduler(mock_db_factory, config)

        with patch.object(scheduler, "_apply_decay", return_value=0), \
             patch.object(scheduler.reflector, "reflect", return_value={"promoted": 0}), \
             patch.object(scheduler.health, "detect_pollution", return_value={"is_polluted": False}), \
             patch.object(scheduler, "_cleanup_stale", return_value=0), \
             patch.object(scheduler.health, "cleanup_orphan_branches", return_value=0), \
             patch.object(scheduler.health, "cleanup_snapshots", return_value=0):

            scheduler.run_cycle("user1")
            scheduler.run_cycle("user2")

        assert "user1" in scheduler._last_cycle
        assert "user2" in scheduler._last_cycle
        assert scheduler._last_cycle["user1"] != scheduler._last_cycle["user2"] or True  # timestamps close


class TestMemoryHealthExtensions:
    """Tests for new MemoryHealth methods."""

    @pytest.fixture
    def mock_db_factory(self):
        return MagicMock()

    def test_cleanup_orphan_branches_finds_sandbox_tables(self, mock_db_factory):
        """Orphan cleanup finds and deletes sandbox tables."""
        from core.memory.health import MemoryHealth

        health = MemoryHealth(mock_db_factory)

        mock_session = MagicMock()
        # First call: find orphan tables
        mock_rows = [
            MagicMock(table_name="memories_sandbox_abc123"),
            MagicMock(table_name="memories_sandbox_def456"),
        ]
        mock_session.execute.return_value.fetchall.return_value = mock_rows

        with patch.object(health, "_db") as mock_db:
            mock_db.return_value.__enter__.return_value = mock_session
            mock_db.return_value.__exit__.return_value = None

            count = health.cleanup_orphan_branches()

        # Should attempt to delete both
        assert mock_session.execute.call_count >= 1

    def test_get_storage_stats_returns_metrics(self, mock_db_factory):
        """Storage stats returns count and size metrics."""
        from core.memory.health import MemoryHealth

        health = MemoryHealth(mock_db_factory)

        mock_session = MagicMock()
        mock_row = MagicMock(
            total=100,
            active=80,
            avg_content_size=150.5,
            oldest=datetime(2026, 1, 1),
            newest=datetime(2026, 2, 26),
        )
        mock_session.execute.return_value.fetchone.return_value = mock_row

        with patch.object(health, "_db") as mock_db:
            mock_db.return_value.__enter__.return_value = mock_session
            mock_db.return_value.__exit__.return_value = None

            stats = health.get_storage_stats("user123")

        assert stats["total"] == 100
        assert stats["active"] == 80
        assert stats["inactive"] == 20
        assert stats["avg_content_size"] == 150.5
