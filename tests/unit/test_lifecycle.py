"""Tests for memory lifecycle governance."""

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
