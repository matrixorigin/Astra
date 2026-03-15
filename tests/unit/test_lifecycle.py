"""Tests for memory lifecycle governance."""

import os
import pytest
from datetime import datetime, timedelta
from unittest.mock import Mock, patch
from sqlalchemy.exc import IntegrityError
from core.context.lifecycle import MemoryGovernanceEngine


class TestMemoryGovernanceEngine:
    """Test memory lifecycle governance."""

    @pytest.fixture
    def mock_db(self):
        """Mock database session."""
        return Mock()

    @pytest.fixture
    def engine(self, mock_db):
        """Create governance engine."""
        return MemoryGovernanceEngine(lambda: mock_db)

    def test_hourly_tasks(self, engine, mock_db):
        """Test hourly governance tasks."""
        # Mock UPDATE: query().filter().update() returns rowcount
        mock_db.query.return_value.filter.return_value.update.return_value = 0

        results = engine.run_hourly_tasks()

        assert "archived_notes" in results
        assert results["archived_notes"] == 0
        assert "sandbox_cleaned" in results

    def test_daily_tasks(self, engine, mock_db):
        """Test daily governance tasks."""
        # Mock quarantine query — no entries below threshold
        mock_db.query.return_value.filter.return_value.all.return_value = []

        results = engine.run_daily_tasks()

        assert "quarantined" in results

    def test_weekly_tasks(self, engine, mock_db):
        """Test weekly governance tasks."""
        # Mock knowledge entries query
        mock_db.query.return_value.filter.return_value.all.return_value = []
        # Mock SQL aggregation for contradictions (empty)
        mock_db.query.return_value.filter.return_value.group_by.return_value.having.return_value.limit.return_value.all.return_value = []
        # Mock health reports GROUP BY query: query().group_by().limit().all()
        mock_db.query.return_value.group_by.return_value.limit.return_value.all.return_value = [
            ("alice", 5, 0.6, 1),
            ("bob", 3, 0.8, 0),
        ]

        results = engine.run_weekly_tasks()

        assert "contradictions_found" in results
        assert results["health_reports"] == 2

    def test_quarantine_low_confidence(self, engine, mock_db):
        """Test quarantine sets confidence to 0 and logs entry_ids."""
        from unittest.mock import MagicMock

        row1 = ("entry_1", "key_a", 0.2)
        row2 = ("entry_2", "key_b", 0.1)
        # First .filter().all() returns rows to quarantine
        mock_db.query.return_value.filter.return_value.all.return_value = [row1, row2]
        # Second .filter().update() applies the change
        mock_db.query.return_value.filter.return_value.update.return_value = 2

        count = engine._quarantine_low_confidence(threshold=0.3)

        assert count == 2
        assert mock_db.commit.call_count >= 1  # quarantine + governance event

    def test_contradiction_scan(self, engine, mock_db):
        """Test contradiction detection."""
        from unittest.mock import Mock

        # Mock SQL aggregation result
        conflict = Mock()
        conflict.category = "user_preference"
        conflict.key_name = "language"
        conflict.val_count = 2

        # Mock chain: query().filter().group_by().having().limit().all()
        mock_db.query.return_value.filter.return_value.group_by.return_value.having.return_value.limit.return_value.all.return_value = [
            conflict
        ]

        # Mock entries fetch for conflict details
        entry1 = Mock(
            entry_id="ke_1", category="user_preference", key_name="language", value="python"
        )
        entry2 = Mock(
            entry_id="ke_2", category="user_preference", key_name="language", value="typescript"
        )
        mock_db.query.return_value.filter.return_value.limit.return_value.all.return_value = [
            entry1,
            entry2,
        ]

        # Dedup check returns empty (no existing contradiction events)
        mock_db.execute.return_value.fetchall.return_value = []

        count = engine._scan_contradictions()

        assert count == 1

    def test_memory_health_stats(self, engine, mock_db):
        """Test memory health statistics via aggregate query."""
        # Mock aggregate: query(count, avg, sum_case).filter().first()
        mock_row = (3, 0.533, 1)  # total=3, avg=0.533, low_confidence=1
        mock_db.query.return_value.filter.return_value.first.return_value = mock_row

        stats = engine._get_user_memory_stats("alice")

        assert stats["total_entries"] == 3
        assert stats["avg_confidence"] == pytest.approx(0.533, rel=0.01)
        assert stats["low_confidence"] == 1


class TestGovernanceTaskRunner:
    """Test distributed task runner with table-based locking."""

    @pytest.fixture
    def mock_db_ctx(self):
        """Mock db context factory returning (factory, db)."""
        from contextlib import contextmanager

        db = Mock()
        # Cover all query chain patterns used by MemoryGovernanceEngine
        q = db.query.return_value
        q.filter.return_value.all.return_value = []
        q.filter.return_value.limit.return_value.all.return_value = []
        q.filter.return_value.count.return_value = 0
        q.filter.return_value.first.return_value = None
        q.filter.return_value.filter.return_value.all.return_value = []
        q.filter.return_value.filter.return_value.count.return_value = 0
        q.all.return_value = []
        q.count.return_value = 0
        # execute() returns mock with fetchall/fetchone
        exec_result = Mock()
        exec_result.fetchall.return_value = []
        exec_result.fetchone.return_value = None
        exec_result.rowcount = 0
        exec_result.scalar.return_value = 0
        db.execute.return_value = exec_result

        @contextmanager
        def factory():
            yield db

        return factory, db

    def test_run_acquires_lock(self, mock_db_ctx):
        """Task runner acquires lock via INSERT and executes."""
        from core.context.scheduler import GovernanceTaskRunner

        factory, db = mock_db_ctx
        runner = GovernanceTaskRunner(factory)
        with patch("api.database.SessionLocal", return_value=db):
            result = runner.run("hourly")

        assert result is not None
        assert "archived_notes" in result

    def test_run_skips_when_lock_held(self, mock_db_ctx):
        """Task runner skips when INSERT fails and lock not expired (CAS returns 0 rows)."""
        pytest.skip("Requires real DB for lock testing - covered by integration tests")

    def test_run_takes_expired_lock(self, mock_db_ctx):
        """Task runner takes over expired lock via atomic CAS UPDATE."""
        from core.context.scheduler import GovernanceTaskRunner

        factory, db = mock_db_ctx
        # INSERT fails (lock exists)
        db.add.side_effect = IntegrityError("Duplicate key", params=None, orig=None)
        # CAS UPDATE matches 1 row (lock expired, takeover succeeds)
        cas_result = Mock()
        cas_result.rowcount = 1
        db.execute.return_value = cas_result

        runner = GovernanceTaskRunner(factory)
        with patch("api.database.SessionLocal", return_value=db):
            result = runner.run("hourly")

        assert result is not None
        assert "archived_notes" in result

    def test_run_rollback_on_task_error(self, mock_db_ctx):
        """Task runner catches lifecycle error and still runs memory governance."""
        pytest.skip("Behavior changed - now raises on error, covered by integration tests")

    def test_governance_disabled_via_env(self):
        """Scheduler respects GOVERNANCE_ENABLED=false."""
        from core.context.scheduler import MemoryGovernanceScheduler

        import asyncio

        with patch.dict(os.environ, {"GOVERNANCE_ENABLED": "false"}):
            scheduler = MemoryGovernanceScheduler()
            asyncio.run(scheduler.start())
            # Should be a no-op, no backend created
            assert scheduler._backend is None


class TestSchedulerBackendInterface:
    """Test that custom backends can be plugged in."""

    def test_custom_backend(self):
        """MemoryGovernanceScheduler accepts any SchedulerBackend."""
        from core.context.scheduler import MemoryGovernanceScheduler, SchedulerBackend

        class StubBackend(SchedulerBackend):
            started = False

            async def start(self, tasks):
                self.started = True

            async def stop(self):
                self.started = False

        backend = StubBackend()
        scheduler = MemoryGovernanceScheduler(backend=backend)

        import asyncio

        asyncio.run(scheduler.start())
        assert backend.started
        asyncio.run(scheduler.stop())
        assert not backend.started
