"""Unit tests for MemoryProvenance and MemoryHealth — Task 7."""

from datetime import datetime, timedelta
from unittest.mock import MagicMock

import pytest

from core.memory.provenance import MemoryProvenance
from core.memory.health import MemoryHealth


# ---------------------------------------------------------------------------
# Provenance
# ---------------------------------------------------------------------------

@pytest.fixture
def mock_db_prov():
    db = MagicMock()
    db.execute.return_value.fetchall.return_value = []
    db.execute.return_value.fetchone.return_value = None
    return db


@pytest.fixture
def provenance(mock_db_prov):
    return MemoryProvenance(db_factory=lambda: mock_db_prov, db_name="test_db")


class TestMemoryStateAt:
    def test_returns_memories(self, provenance, mock_db_prov):
        mock_db_prov.execute.return_value.fetchall.return_value = [
            MagicMock(memory_id="m1", content="test", memory_type="profile",
                     confidence=0.8, observed_at=datetime(2026, 2, 26)),
        ]
        result = provenance.memory_state_at("u1", datetime(2026, 2, 25))
        assert len(result) == 1
        assert result[0]["memory_id"] == "m1"


class TestChangesAround:
    def test_returns_window(self, provenance, mock_db_prov):
        mock_db_prov.execute.return_value.fetchall.return_value = [
            MagicMock(memory_id="m1", content="a", memory_type="episodic",
                     observed_at=datetime(2026, 2, 26, 12, 0)),
            MagicMock(memory_id="m2", content="b", memory_type="episodic",
                     observed_at=datetime(2026, 2, 26, 12, 3)),
        ]
        result = provenance.changes_around("u1", datetime(2026, 2, 26, 12, 0), 300)
        assert len(result) == 2


class TestRollbackBeforeMemory:
    def test_rolls_back(self, provenance, mock_db_prov):
        mock_db_prov.execute.return_value.fetchone.return_value = MagicMock(
            observed_at=datetime(2026, 2, 26, 12, 0)
        )
        # Mock the rollback call
        provenance.rollback_to_timestamp = MagicMock(return_value=True)
        result = provenance.rollback_before_memory("m1")
        assert result is True
        provenance.rollback_to_timestamp.assert_called_once()

    def test_returns_false_if_not_found(self, provenance, mock_db_prov):
        mock_db_prov.execute.return_value.fetchone.return_value = None
        assert provenance.rollback_before_memory("nonexistent") is False


class TestTraceSource:
    def test_returns_event_ids(self, provenance, mock_db_prov):
        mock_db_prov.execute.return_value.fetchone.return_value = MagicMock(
            source_event_ids='["e1", "e2"]'
        )
        result = provenance.trace_source("m1")
        assert result == ["e1", "e2"]

    def test_returns_empty_if_none(self, provenance, mock_db_prov):
        mock_db_prov.execute.return_value.fetchone.return_value = None
        assert provenance.trace_source("m1") == []


class TestCreateMilestone:
    def test_creates_snapshot(self, provenance, mock_db_prov):
        # Mock the raw connection for DDL
        mock_raw = MagicMock()
        mock_db_prov.connection.return_value.connection = mock_raw
        mock_raw.cursor.return_value = MagicMock()

        name = provenance.create_milestone("test_snap")
        assert name == "test_snap"
        mock_raw.autocommit.assert_called()
        mock_raw.cursor.return_value.execute.assert_called()


# ---------------------------------------------------------------------------
# Health
# ---------------------------------------------------------------------------

@pytest.fixture
def mock_db_health():
    db = MagicMock()
    db.execute.return_value.fetchall.return_value = []
    db.execute.return_value.fetchone.return_value = None
    return db


@pytest.fixture
def health(mock_db_health):
    return MemoryHealth(db_factory=lambda: mock_db_health, db_name="test_db")


class TestAnalyze:
    def test_returns_stats(self, health, mock_db_health):
        mock_db_health.execute.return_value.fetchall.return_value = [
            MagicMock(memory_type="profile", total=10, avg_confidence=0.8,
                     superseded=2, avg_staleness_hours=24),
            MagicMock(memory_type="episodic", total=50, avg_confidence=0.6,
                     superseded=10, avg_staleness_hours=48),
        ]
        result = health.analyze("u1")
        assert "profile" in result
        assert result["profile"]["total"] == 10
        assert result["profile"]["contradiction_rate"] == 0.2


class TestDetectPollution:
    def test_detects_high_supersede_ratio(self, health, mock_db_health):
        mock_db_health.execute.return_value.fetchone.return_value = MagicMock(
            total_changes=10, supersedes=5
        )
        health.pollution_threshold = 0.3
        result = health.detect_pollution("u1", datetime(2026, 2, 26))
        assert result["is_polluted"] is True
        assert result["ratio"] == 0.5

    def test_no_pollution_when_low_ratio(self, health, mock_db_health):
        mock_db_health.execute.return_value.fetchone.return_value = MagicMock(
            total_changes=10, supersedes=1
        )
        result = health.detect_pollution("u1", datetime(2026, 2, 26))
        assert result["is_polluted"] is False


class TestSuggestRollbackTarget:
    def test_returns_low_confidence_memory(self, health, mock_db_health):
        mock_db_health.execute.return_value.fetchone.return_value = MagicMock(
            memory_id="bad_mem"
        )
        result = health.suggest_rollback_target("u1")
        assert result == "bad_mem"

    def test_returns_none_if_all_good(self, health, mock_db_health):
        mock_db_health.execute.return_value.fetchone.return_value = None
        assert health.suggest_rollback_target("u1") is None


class TestCleanupSnapshots:
    def test_drops_old_snapshots(self, health, mock_db_health):
        mock_db_health.execute.return_value.fetchall.return_value = [
            MagicMock(sname="mem_milestone_1"),
            MagicMock(sname="mem_milestone_2"),
            MagicMock(sname="mem_milestone_3"),
            MagicMock(sname="mem_milestone_4"),
            MagicMock(sname="mem_milestone_5"),
            MagicMock(sname="mem_milestone_6"),
            MagicMock(sname="mem_milestone_7"),
        ]
        dropped = health.cleanup_snapshots(keep_last_n=5)
        assert dropped == 2

    def test_no_drop_when_under_limit(self, health, mock_db_health):
        mock_db_health.execute.return_value.fetchall.return_value = [
            MagicMock(sname="mem_milestone_1"),
            MagicMock(sname="mem_milestone_2"),
        ]
        dropped = health.cleanup_snapshots(keep_last_n=5)
        assert dropped == 0
