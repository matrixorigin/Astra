"""Tests for memory lifecycle governance."""

import os
import pytest
from datetime import datetime, timedelta
from unittest.mock import Mock, patch
from core.context.lifecycle import MemoryGovernanceEngine, TRUST_TIER_HALF_LIVES


class TestMemoryGovernanceEngine:
    """Test memory lifecycle governance."""
    
    @pytest.fixture
    def mock_db(self):
        """Mock database session."""
        return Mock()
    
    @pytest.fixture
    def engine(self, mock_db):
        """Create governance engine."""
        return MemoryGovernanceEngine(mock_db)
    
    def test_hourly_tasks(self, engine, mock_db):
        """Test hourly governance tasks."""
        # Mock scratchpad query
        mock_db.query.return_value.filter.return_value.all.return_value = []
        
        results = engine.run_hourly_tasks()
        
        assert "archived_notes" in results
        assert results["archived_notes"] == 0
        assert "sandbox_cleaned" in results
    
    def test_daily_tasks(self, engine, mock_db):
        """Test daily governance tasks."""
        # Mock knowledge entries query
        mock_db.query.return_value.filter.return_value.all.return_value = []
        # Mock events query for compression
        mock_db.query.return_value.filter.return_value.limit.return_value.all.return_value = []
        
        results = engine.run_daily_tasks()
        
        assert "decayed_entries" in results
        assert "quarantined" in results
        assert "compressed_events" in results
    
    def test_weekly_tasks(self, engine, mock_db):
        """Test weekly governance tasks."""
        # Mock knowledge entries query
        mock_db.query.return_value.filter.return_value.all.return_value = []
        # Mock distinct users query
        from sqlalchemy import distinct
        mock_db.query.return_value.all.return_value = [("alice",), ("bob",)]
        
        results = engine.run_weekly_tasks()
        
        assert "contradictions_found" in results
        assert "health_reports" in results
    
    def test_confidence_decay_calculation(self, engine, mock_db):
        """Test confidence decay formula."""
        # Create mock entry
        entry = Mock()
        entry.trust_tier = "T3"
        entry.initial_confidence = 1.0
        entry.confidence = 1.0
        entry.last_validated_at = datetime.now() - timedelta(days=60)
        
        mock_db.query.return_value.filter.return_value.all.return_value = [entry]
        
        count = engine._apply_confidence_decay()
        
        # After 60 days with T3 half-life (60 days), confidence should be 0.5
        assert entry.confidence == pytest.approx(0.5, rel=0.01)
        assert count == 1
    
    def test_quarantine_low_confidence(self, engine, mock_db):
        """Test quarantine of low confidence entries."""
        # Create mock entries
        low_entry = Mock()
        low_entry.entry_id = "ke_low"
        low_entry.key_name = "test_key"
        low_entry.confidence = 0.2
        low_entry.trust_tier = "T4"
        
        mock_db.query.return_value.filter.return_value.all.return_value = [low_entry]
        
        count = engine._quarantine_low_confidence(threshold=0.3)
        
        assert count == 1
    
    def test_contradiction_scan(self, engine, mock_db):
        """Test contradiction detection."""
        # Create mock entries with same key but different values
        entry1 = Mock()
        entry1.entry_id = "ke_1"
        entry1.category = "user_preference"
        entry1.key_name = "language"
        entry1.value = "python"
        entry1.confidence = 0.8
        
        entry2 = Mock()
        entry2.entry_id = "ke_2"
        entry2.category = "user_preference"
        entry2.key_name = "language"
        entry2.value = "typescript"
        entry2.confidence = 0.7
        
        mock_db.query.return_value.filter.return_value.all.return_value = [entry1, entry2]
        
        count = engine._scan_contradictions()
        
        assert count == 1
    
    def test_memory_health_stats(self, engine, mock_db):
        """Test memory health statistics."""
        # Create mock entries
        entries = [
            Mock(confidence=0.8),
            Mock(confidence=0.6),
            Mock(confidence=0.2),
        ]
        
        mock_db.query.return_value.filter.return_value.all.return_value = entries
        
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
        db.query.return_value.filter.return_value.all.return_value = []
        db.query.return_value.filter.return_value.limit.return_value.all.return_value = []
        db.query.return_value.all.return_value = []

        @contextmanager
        def factory():
            yield db

        return factory, db

    def test_run_acquires_lock(self, mock_db_ctx):
        """Task runner acquires lock via INSERT and executes."""
        from core.context.scheduler import GovernanceTaskRunner

        factory, db = mock_db_ctx
        runner = GovernanceTaskRunner(factory)
        result = runner.run("hourly")

        assert result is not None
        assert "archived_notes" in result

    def test_run_skips_when_lock_held(self, mock_db_ctx):
        """Task runner skips when INSERT fails and lock not expired (CAS returns 0 rows)."""
        from core.context.scheduler import GovernanceTaskRunner

        factory, db = mock_db_ctx
        # INSERT fails (lock exists)
        db.add.side_effect = Exception("Duplicate key")
        # CAS UPDATE matches 0 rows (lock not expired)
        cas_result = Mock()
        cas_result.rowcount = 0
        db.execute.return_value = cas_result

        runner = GovernanceTaskRunner(factory)
        result = runner.run("hourly")

        assert result is None

    def test_run_takes_expired_lock(self, mock_db_ctx):
        """Task runner takes over expired lock via atomic CAS UPDATE."""
        from core.context.scheduler import GovernanceTaskRunner

        factory, db = mock_db_ctx
        # INSERT fails (lock exists)
        db.add.side_effect = Exception("Duplicate key")
        # CAS UPDATE matches 1 row (lock expired, takeover succeeds)
        cas_result = Mock()
        cas_result.rowcount = 1
        db.execute.return_value = cas_result

        runner = GovernanceTaskRunner(factory)
        result = runner.run("hourly")

        assert result is not None
        assert "archived_notes" in result

    def test_run_rollback_on_task_error(self, mock_db_ctx):
        """Task runner rolls back DB and releases lock on task exception."""
        from core.context.scheduler import GovernanceTaskRunner

        factory, db = mock_db_ctx

        with patch("core.context.lifecycle.MemoryGovernanceEngine") as MockEngine:
            MockEngine.return_value.run_hourly_tasks.side_effect = RuntimeError("boom")

            runner = GovernanceTaskRunner(factory)
            result = runner.run("hourly")

            assert result is None
            assert db.rollback.called

    def test_governance_disabled_via_env(self):
        """Scheduler respects GOVERNANCE_ENABLED=false."""
        from core.context.scheduler import MemoryGovernanceScheduler

        import asyncio
        with patch.dict(os.environ, {"GOVERNANCE_ENABLED": "false"}):
            scheduler = MemoryGovernanceScheduler()
            asyncio.get_event_loop().run_until_complete(scheduler.start())
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
        asyncio.get_event_loop().run_until_complete(scheduler.start())
        assert backend.started
        asyncio.get_event_loop().run_until_complete(scheduler.stop())
        assert not backend.started
