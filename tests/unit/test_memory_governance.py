"""Unit tests for GovernanceScheduler — frequency-separated governance."""

from datetime import datetime, timedelta
from unittest.mock import MagicMock, patch, call

import pytest

from core.memory.governance import GovernanceScheduler, GovernanceCycleResult
from core.memory.config import MemoryGovernanceConfig


@pytest.fixture
def mock_db():
    db = MagicMock()
    result = MagicMock()
    result.rowcount = 0
    db.execute.return_value = result
    return db


@pytest.fixture
def scheduler(mock_db):
    return GovernanceScheduler(db_factory=lambda: mock_db)


class TestRunHourly:
    def test_returns_result(self, scheduler):
        r = scheduler.run_hourly()
        assert isinstance(r, GovernanceCycleResult)

    def test_cleans_tool_results(self, scheduler, mock_db):
        mock_db.execute.return_value.rowcount = 3
        r = scheduler.run_hourly()
        assert r.cleaned_tool_results == 3

    def test_archives_working_memories(self, scheduler, mock_db):
        # First call = tool_result cleanup (0), second = working archival (2)
        results = [MagicMock(rowcount=0), MagicMock(rowcount=2)]
        mock_db.execute.side_effect = results
        r = scheduler.run_hourly()
        assert r.archived_working == 2

    def test_error_captured(self, scheduler, mock_db):
        mock_db.execute.side_effect = Exception("db down")
        r = scheduler.run_hourly()
        assert len(r.errors) >= 1


class TestRunDaily:
    def test_returns_result(self, scheduler):
        r = scheduler.run_daily("u1")
        assert isinstance(r, GovernanceCycleResult)

    def test_cleans_stale(self, scheduler, mock_db):
        mock_db.execute.return_value.rowcount = 5
        r = scheduler.run_daily("u1")
        assert r.cleaned_stale == 5

    def test_quarantines_low_confidence(self, scheduler, mock_db):
        # stale cleanup returns 0, then 4 quarantine calls (one per tier) each return 1
        results = [MagicMock(rowcount=0)]  # stale
        for _ in range(4):
            results.append(MagicMock(rowcount=1))  # quarantine per tier
        mock_db.execute.side_effect = results
        r = scheduler.run_daily("u1")
        assert r.quarantined == 4


class TestRunWeekly:
    def test_returns_result(self, scheduler):
        with patch.object(scheduler.health, "cleanup_orphan_branches", return_value=0), \
             patch.object(scheduler.health, "cleanup_snapshots", return_value=0):
            r = scheduler.run_weekly()
        assert isinstance(r, GovernanceCycleResult)


class TestRunCycle:
    def test_calls_all_frequencies(self, scheduler):
        with patch.object(scheduler, "run_hourly", return_value=GovernanceCycleResult(cleaned_tool_results=1)), \
             patch.object(scheduler, "run_daily", return_value=GovernanceCycleResult(cleaned_stale=2)), \
             patch.object(scheduler, "run_weekly", return_value=GovernanceCycleResult(cleaned_branches=3)):
            r = scheduler.run_cycle("u1")
        assert r.cleaned_tool_results == 1
        assert r.cleaned_stale == 2
        assert r.cleaned_branches == 3


class TestNoDecayMutation:
    def test_governance_does_not_mutate_confidence(self, scheduler, mock_db):
        """Governance never writes to initial_confidence column."""
        scheduler.run_hourly()
        for c in mock_db.execute.call_args_list:
            sql = str(c)
            assert "initial_confidence =" not in sql or "SET" not in sql


class TestQuarantineConfig:
    def test_custom_threshold(self, mock_db):
        config = MemoryGovernanceConfig(quarantine_threshold=0.5)
        s = GovernanceScheduler(db_factory=lambda: mock_db, config=config)
        assert s.config.quarantine_threshold == 0.5


class TestWorkingMemoryStaleConfig:
    def test_custom_stale_hours(self, mock_db):
        config = MemoryGovernanceConfig(working_memory_stale_hours=4)
        s = GovernanceScheduler(db_factory=lambda: mock_db, config=config)
        assert s.config.working_memory_stale_hours == 4
