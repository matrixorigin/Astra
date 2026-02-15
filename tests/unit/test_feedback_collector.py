"""Tests for _FeedbackBuffer and SkillLearningSignal model."""

import pytest
import uuid
import json
from datetime import datetime, timezone
from unittest.mock import MagicMock
from core.skills.pipeline import _FeedbackBuffer, SignalType
from api.models import SkillLearningSignal


@pytest.fixture
def db():
    """Mock database session."""
    mock_db = MagicMock()
    mock_db.query.return_value.filter.return_value.all.return_value = []
    return mock_db


@pytest.fixture
def buffer(db):
    """Create _FeedbackBuffer instance."""
    return _FeedbackBuffer(db, batch_size=2, flush_interval=60)


class TestFeedbackBuffer:
    """Test _FeedbackBuffer functionality."""

    def test_init(self, buffer):
        """Test buffer initialization."""
        assert buffer._batch_size == 2
        assert buffer._flush_interval == 60
        assert len(buffer._buffer) == 0

    def test_add_signal(self, buffer):
        """Test adding a single signal."""
        event_id = f"evt-{uuid.uuid4().hex[:8]}"
        data = {"skill": "wrong_skill", "reason": "test"}
        
        buffer.add(event_id, SignalType.WRONG_SKILL, data)
        
        assert len(buffer._buffer) == 1
        signal = buffer._buffer[0]
        assert signal["selection_event_id"] == event_id
        assert signal["signal_type"] == SignalType.WRONG_SKILL.value
        assert json.loads(signal["signal_data"]) == data

    def test_auto_flush_on_batch_size(self, buffer, db):
        """Test automatic flush when batch size reached."""
        event_id1 = f"evt-{uuid.uuid4().hex[:8]}"
        event_id2 = f"evt-{uuid.uuid4().hex[:8]}"
        
        buffer.add(event_id1, SignalType.WRONG_SKILL, {"test": 1})
        assert len(buffer._buffer) == 1
        assert db.execute.call_count == 0
        
        buffer.add(event_id2, SignalType.SLOW_EXECUTION, {"test": 2})
        # Should auto-flush after 2nd record (batch_size=2)
        assert db.execute.call_count == 2
        assert db.commit.call_count == 1
        assert len(buffer._buffer) == 0

    def test_manual_flush(self, buffer, db):
        """Test manual flush."""
        event_id = f"evt-{uuid.uuid4().hex[:8]}"
        buffer.add(event_id, SignalType.HIGH_COST, {"cost": 0.5})
        
        assert len(buffer._buffer) == 1
        flushed = buffer.flush()
        
        assert flushed == 1
        assert db.execute.call_count == 1
        assert db.commit.call_count == 1
        assert len(buffer._buffer) == 0

    def test_flush_empty_buffer(self, buffer, db):
        """Test flushing empty buffer."""
        flushed = buffer.flush()
        
        assert flushed == 0
        assert db.execute.call_count == 0

    def test_flush_error_requeues(self, buffer, db):
        """Test that flush errors re-queue signals."""
        db.execute.side_effect = Exception("DB error")
        
        event_id = f"evt-{uuid.uuid4().hex[:8]}"
        buffer.add(event_id, SignalType.WRONG_SKILL, {"test": 1})
        buffer.add(event_id, SignalType.SLOW_EXECUTION, {"test": 2})
        
        # Auto-flush should fail and re-queue
        assert len(buffer._buffer) == 2
        assert db.rollback.call_count == 1

    def test_maybe_flush_interval_not_elapsed(self, buffer, db):
        """Test maybe_flush when interval not elapsed."""
        event_id = f"evt-{uuid.uuid4().hex[:8]}"
        buffer.add(event_id, SignalType.WRONG_SKILL, {"test": 1})
        
        flushed = buffer.maybe_flush()
        
        assert flushed == 0
        assert db.execute.call_count == 0
        assert len(buffer._buffer) == 1

    def test_signal_id_uniqueness(self, db):
        """Test that each signal gets unique ID."""
        # Use larger batch_size to prevent auto-flush
        buffer = _FeedbackBuffer(db, batch_size=10, flush_interval=60)
        event_id = f"evt-{uuid.uuid4().hex[:8]}"
        
        buffer.add(event_id, SignalType.WRONG_SKILL, {"test": 1})
        buffer.add(event_id, SignalType.WRONG_SKILL, {"test": 2})
        
        signal_ids = [s["signal_id"] for s in buffer._buffer]
        assert len(signal_ids) == 2
        assert signal_ids[0] != signal_ids[1]

    def test_all_signal_types(self, db):
        """Test all signal types."""
        # Use larger batch_size to prevent auto-flush
        buffer = _FeedbackBuffer(db, batch_size=10, flush_interval=60)
        event_id = f"evt-{uuid.uuid4().hex[:8]}"
        
        signal_types = [
            SignalType.WRONG_SKILL,
            SignalType.SLOW_EXECUTION,
            SignalType.HIGH_COST,
            SignalType.LOW_SATISFACTION
        ]
        for signal_type in signal_types:
            buffer.add(event_id, signal_type, {"type": signal_type.value})
        
        assert len(buffer._buffer) == 4
        recorded_types = [s["signal_type"] for s in buffer._buffer]
        assert recorded_types == [st.value for st in signal_types]


class TestSkillLearningSignalModel:
    """Test SkillLearningSignal database model."""

    def test_model_attributes(self):
        """Test model has required attributes."""
        assert hasattr(SkillLearningSignal, '__tablename__')
        assert SkillLearningSignal.__tablename__ == 'skill_learning_signals'
        
        columns = [c.name for c in SkillLearningSignal.__table__.columns]
        assert 'signal_id' in columns
        assert 'selection_event_id' in columns
        assert 'signal_type' in columns
        assert 'signal_data' in columns
        assert 'created_at' in columns

    def test_model_primary_key(self):
        """Test primary key configuration."""
        pk_cols = [c.name for c in SkillLearningSignal.__table__.primary_key.columns]
        assert pk_cols == ['signal_id']

    def test_model_indexes(self):
        """Test index configuration."""
        indexes = {idx.name: [col.name for col in idx.columns] 
                   for idx in SkillLearningSignal.__table__.indexes}
        
        # Should have indexes on selection_event_id, signal_type, created_at
        indexed_columns = set()
        for cols in indexes.values():
            indexed_columns.update(cols)
        
        assert 'selection_event_id' in indexed_columns
        assert 'signal_type' in indexed_columns
        assert 'created_at' in indexed_columns
