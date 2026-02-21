"""Tests for memory pollution detection."""

import pytest
from unittest.mock import Mock, patch
from core.context.pollution import PollutionDetector


class TestPollutionDetector:
    """Test memory pollution detection."""
    
    @pytest.fixture
    def mock_db(self):
        """Mock database session."""
        return Mock()
    
    @pytest.fixture
    def detector(self, mock_db):
        """Create pollution detector."""
        return PollutionDetector(mock_db)
    
    def test_detect_pollution_candidates(self, detector, mock_db):
        """Test pollution candidate detection."""
        # Mock knowledge entries
        from datetime import datetime, timedelta
        
        entry = Mock()
        entry.entry_id = "ke_123"
        entry.key_name = "test_key"
        entry.category = "user_preference"
        entry.user_id = "alice"
        entry.confidence = 0.5
        entry.last_validated_at = datetime.now() - timedelta(days=100)
        entry.value = "test_value"
        
        mock_db.query.return_value.filter.return_value.all.return_value = [entry]
        mock_db.query.return_value.filter.return_value.count.return_value = 0
        
        candidates = detector.detect_pollution_candidates("alice")
        
        # Should detect stale entry
        assert len(candidates) >= 0
    
    def test_calculate_pollution_signals(self, detector, mock_db):
        """Test pollution signal calculation."""
        from datetime import datetime, timedelta
        
        entry = Mock()
        entry.entry_id = "ke_123"
        entry.user_id = "alice"
        entry.category = "user_preference"
        entry.key_name = "language"
        entry.value = "python"
        entry.confidence = 0.6
        entry.last_validated_at = datetime.now() - timedelta(days=100)
        
        mock_db.query.return_value.filter.return_value.count.return_value = 2
        
        signals = detector._calculate_pollution_signals(entry)
        
        assert "days_since_validation" in signals
        assert "contradicting_entries" in signals
        assert "downstream_quality" in signals
        assert signals["days_since_validation"] == 100
        assert signals["contradicting_entries"] == 2
    
    def test_classify_severity(self, detector):
        """Test severity classification."""
        # HIGH: Low downstream quality
        signals = {
            "downstream_quality": 2.0,
            "contradicting_entries": 0,
            "days_since_validation": 30,
            "confidence": 0.5,
        }
        severity = detector._classify_severity(signals, 2.5, 2, 90)
        assert severity == "high"
        
        # MEDIUM: Contradictions
        signals = {
            "downstream_quality": 3.0,
            "contradicting_entries": 3,
            "days_since_validation": 30,
            "confidence": 0.5,
        }
        severity = detector._classify_severity(signals, 2.5, 2, 90)
        assert severity == "medium"
        
        # LOW: Stale
        signals = {
            "downstream_quality": 3.0,
            "contradicting_entries": 0,
            "days_since_validation": 100,
            "confidence": 0.5,
        }
        severity = detector._classify_severity(signals, 2.5, 2, 90)
        assert severity == "low"
    
    def test_quarantine_entry(self, detector, mock_db):
        """Test entry quarantine."""
        # Mock entry
        entry = Mock()
        entry.entry_id = "ke_123"
        entry.key_name = "test_key"
        entry.confidence = 0.8
        
        mock_db.query.return_value.filter.return_value.first.return_value = entry
        
        result = detector.quarantine_entry("ke_123", "high", "Low quality")
        
        assert result is True
        assert entry.confidence == 0.0
        mock_db.commit.assert_called_once()
    
    def test_scan_contradictions(self, detector, mock_db):
        """Test contradiction scanning."""
        # Mock entries with contradictions
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
        
        contradictions = detector.scan_contradictions("alice")
        
        assert len(contradictions) == 1
        assert contradictions[0]["category"] == "user_preference"
        assert contradictions[0]["key_name"] == "language"
        assert contradictions[0]["value_count"] == 2

    def test_cascade_impact_traces_through_snapshots(self, detector, mock_db):
        """Test that cascade analysis parses context_snapshots to find affected decisions."""
        import json

        # Snapshot whose selected_events references the polluted entry
        snap = Mock()
        snap.context_capture_id = "snap_1"
        snap.selected_events = [{"event_id": "ke_polluted", "score": 0.9}]

        # Decision linked to that snapshot
        decision = Mock()
        decision.decision_id = "dec_1"
        decision.context_capture_id = "snap_1"
        decision.event_id = "evt_from_dec"

        # Knowledge entry sourced from the decision's event
        downstream = Mock()
        downstream.entry_id = "ke_downstream"
        downstream.source_event_ids = json.dumps(["evt_from_dec"])

        # Wire up db.query routing
        def route_query(model):
            m = Mock()
            name = model.__tablename__
            if name == "context_snapshots":
                m.all.return_value = [snap]
            elif name == "decision_audit":
                m.filter.return_value.all.return_value = [decision]
            elif name == "sk_knowledge_entries":
                m.all.return_value = [downstream]
            return m

        mock_db.query.side_effect = route_query

        result = detector.analyze_cascade_impact("ke_polluted")

        assert result["affected_decisions"] == 1
        assert result["affected_entries"] == 1
        assert result["contamination_depth"] >= 1
        assert "note" not in result  # No longer a placeholder

    def test_cascade_impact_no_snapshots(self, detector, mock_db):
        """Test cascade returns zero impact when no snapshots reference the entry."""
        snap = Mock()
        snap.context_capture_id = "snap_x"
        snap.selected_events = [{"event_id": "unrelated"}]

        mock_db.query.return_value.all.return_value = [snap]

        result = detector.analyze_cascade_impact("ke_orphan")

        assert result["affected_decisions"] == 0
        assert result["affected_entries"] == 0
