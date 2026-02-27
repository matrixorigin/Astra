"""Unit tests for GovernanceScheduler."""

from datetime import datetime, timedelta
from unittest.mock import MagicMock, patch

import pytest

from core.memory.governance import GovernanceScheduler, GovernanceCycleResult, GovernanceStepStats
from core.memory.config import MemoryGovernanceConfig


class TestGovernanceScheduler:

    @pytest.fixture
    def mock_db_factory(self):
        return MagicMock()

    @pytest.fixture
    def config(self):
        return MemoryGovernanceConfig(
            pollution_threshold=0.3,
            milestone_snapshot_keep_n=5,
        )

    @pytest.fixture
    def scheduler(self, mock_db_factory, config):
        return GovernanceScheduler(mock_db_factory, config)

    def test_cleanup_stale_deletes_low_confidence_inactive(self, scheduler):
        """Cleanup removes inactive memories below threshold."""
        mock_session = MagicMock()
        mock_session.execute.return_value.rowcount = 3

        with patch.object(scheduler, "_db") as mock_db:
            mock_db.return_value.__enter__.return_value = mock_session
            mock_db.return_value.__exit__.return_value = None
            count = scheduler._cleanup_stale("user123", confidence_threshold=0.1)

        assert count == 3
        sql_text = str(mock_session.execute.call_args[0][0])
        assert "DELETE FROM memories" in sql_text
        assert "is_active = 0" in sql_text
        assert "initial_confidence <" in sql_text

    def test_cleanup_tool_results_respects_ttl(self, scheduler):
        """Tool result cleanup uses configured TTL."""
        mock_session = MagicMock()
        mock_session.execute.return_value.rowcount = 2

        with patch.object(scheduler, "_db") as mock_db:
            mock_db.return_value.__enter__.return_value = mock_session
            mock_db.return_value.__exit__.return_value = None
            count = scheduler._cleanup_tool_results()

        assert count == 2
        sql_text = str(mock_session.execute.call_args[0][0])
        assert "memory_type = :mtype" in sql_text
        assert "TIMESTAMPDIFF" in sql_text

    def test_run_cycle_executes_all_steps(self, scheduler):
        """Full cycle runs health, stale cleanup, branches, snapshots, tool_results."""
        with patch.object(scheduler, "_health_check") as mock_health, \
             patch.object(scheduler, "_cleanup_stale", return_value=3) as mock_stale, \
             patch.object(scheduler.health, "cleanup_orphan_branches", return_value=0), \
             patch.object(scheduler.health, "cleanup_snapshots", return_value=1), \
             patch.object(scheduler, "_cleanup_tool_results", return_value=2):

            result = scheduler.run_cycle("user123")

        assert result.cleaned_stale == 3
        assert result.cleaned_branches == 0
        assert result.cleaned_snapshots == 1
        assert result.cleaned_tool_results == 2
        assert len(result.errors) == 0
        mock_stale.assert_called_once_with("user123")

    def test_run_cycle_detects_pollution(self, scheduler):
        """Cycle flags pollution when threshold exceeded."""
        with patch.object(scheduler.health, "detect_pollution",
                          return_value={"is_polluted": True, "ratio": 0.5}), \
             patch.object(scheduler, "_cleanup_stale", return_value=0), \
             patch.object(scheduler.health, "cleanup_orphan_branches", return_value=0), \
             patch.object(scheduler.health, "cleanup_snapshots", return_value=0), \
             patch.object(scheduler, "_cleanup_tool_results", return_value=0):

            result = scheduler.run_cycle("user123")

        assert result.pollution_detected is True

    def test_run_cycle_continues_on_error(self, scheduler):
        """Cycle continues even if one step fails."""
        with patch.object(scheduler, "_health_check", side_effect=Exception("health error")), \
             patch.object(scheduler, "_cleanup_stale", return_value=2), \
             patch.object(scheduler.health, "cleanup_orphan_branches", return_value=0), \
             patch.object(scheduler.health, "cleanup_snapshots", return_value=0), \
             patch.object(scheduler, "_cleanup_tool_results", return_value=0):

            result = scheduler.run_cycle("user123")

        assert result.cleaned_stale == 2
        assert any("health" in e for e in result.errors)

    def test_last_cycle_tracked_per_user(self, scheduler):
        """Each user has independent last_cycle timestamp."""
        with patch.object(scheduler, "_health_check"), \
             patch.object(scheduler, "_cleanup_stale", return_value=0), \
             patch.object(scheduler.health, "cleanup_orphan_branches", return_value=0), \
             patch.object(scheduler.health, "cleanup_snapshots", return_value=0), \
             patch.object(scheduler, "_cleanup_tool_results", return_value=0):

            scheduler.run_cycle("user1")
            scheduler.run_cycle("user2")

        assert "user1" in scheduler._last_cycle
        assert "user2" in scheduler._last_cycle

    def test_explain_populates_step_stats(self, scheduler):
        """explain=True populates detailed stats for each step."""
        with patch.object(scheduler, "_health_check"), \
             patch.object(scheduler, "_cleanup_stale", return_value=3), \
             patch.object(scheduler.health, "cleanup_orphan_branches", return_value=0), \
             patch.object(scheduler.health, "cleanup_snapshots", return_value=1), \
             patch.object(scheduler, "_cleanup_tool_results", return_value=2):

            result = scheduler.run_cycle("user123", explain=True)

        assert result.health_stats is not None
        assert result.health_stats.executed is True
        assert result.health_stats.success is True

        assert result.cleanup_stale_stats.success is True
        assert result.cleanup_stale_stats.count == 3

        assert result.cleanup_snapshots_stats.count == 1
        assert result.cleanup_tool_results_stats.count == 2
        assert result.total_ms > 0

    def test_explain_captures_step_errors(self, scheduler):
        """explain=True captures error details per step."""
        with patch.object(scheduler, "_health_check", side_effect=Exception("health failed")), \
             patch.object(scheduler, "_cleanup_stale", return_value=0), \
             patch.object(scheduler.health, "cleanup_orphan_branches", return_value=0), \
             patch.object(scheduler.health, "cleanup_snapshots", return_value=0), \
             patch.object(scheduler, "_cleanup_tool_results", return_value=0):

            result = scheduler.run_cycle("user123", explain=True)

        assert result.health_stats.executed is True
        assert result.health_stats.success is False
        assert result.health_stats.error == "health failed"
        assert result.cleanup_stale_stats.success is True

    def test_explain_false_no_stats(self, scheduler):
        """explain=False leaves stats as None."""
        with patch.object(scheduler, "_health_check"), \
             patch.object(scheduler, "_cleanup_stale", return_value=0), \
             patch.object(scheduler.health, "cleanup_orphan_branches", return_value=0), \
             patch.object(scheduler.health, "cleanup_snapshots", return_value=0), \
             patch.object(scheduler, "_cleanup_tool_results", return_value=0):

            result = scheduler.run_cycle("user123", explain=False)

        assert result.health_stats is None
        assert result.cleanup_stale_stats is None
        assert result.total_ms == 0

    def test_no_decay_mutation(self, scheduler):
        """Governance no longer has _apply_decay — decay is query-time only."""
        assert not hasattr(scheduler, "_apply_decay")

    def test_no_reflector(self, scheduler):
        """Governance no longer has reflector — episodic type eliminated."""
        assert not hasattr(scheduler, "reflector")


class TestGovernanceCycleResult:

    def test_defaults(self):
        r = GovernanceCycleResult()
        assert r.cleaned_stale == 0
        assert r.cleaned_tool_results == 0
        assert r.pollution_detected is False
        assert r.errors == []
        assert r.health_stats is None


class TestGovernanceStepStats:

    def test_defaults(self):
        s = GovernanceStepStats()
        assert s.executed is False
        assert s.success is False
        assert s.error is None
        assert s.count == 0
        assert s.elapsed_ms == 0.0
