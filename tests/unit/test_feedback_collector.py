"""Tests for _FeedbackBuffer and SkillLearningSignal model."""

import pytest
import uuid
import json
from datetime import datetime, timezone
from unittest.mock import MagicMock
from core.skills.pipeline import _FeedbackBuffer, SignalType
from api.models import SkillLearningSignal
from core.utils.id_generator import generate_event_id


@pytest.fixture
def db():
    """Mock database session with get_bind() for independent connection."""
    mock_db = MagicMock()
    mock_db.query.return_value.filter.return_value.all.return_value = []
    # _FeedbackBuffer._flush_locked uses db.get_bind().connect() for thread safety
    mock_conn = MagicMock()
    mock_conn.__enter__ = MagicMock(return_value=mock_conn)
    mock_conn.__exit__ = MagicMock(return_value=False)
    mock_db.get_bind.return_value.connect.return_value = mock_conn
    mock_db._mock_conn = mock_conn  # expose for assertions
    return mock_db


@pytest.fixture
def buffer(db):
    """Create _FeedbackBuffer instance."""
    return _FeedbackBuffer(lambda: db, batch_size=2, flush_interval=60)


class TestFeedbackBuffer:
    """Test _FeedbackBuffer functionality."""

    def test_init(self, buffer):
        """Test buffer initialization."""
        assert buffer._batch_size == 2
        assert buffer._flush_interval == 60
        assert len(buffer._buffer) == 0

    def test_add_signal(self, buffer):
        """Test adding a single signal."""
        event_id = generate_event_id()
        data = {"skill": "wrong_skill", "reason": "test"}
        
        buffer.add(event_id, SignalType.WRONG_SKILL, data)
        
        assert len(buffer._buffer) == 1
        signal = buffer._buffer[0]
        assert signal["selection_event_id"] == event_id
        assert signal["signal_type"] == SignalType.WRONG_SKILL.value
        assert json.loads(signal["signal_data"]) == data

    def test_auto_flush_on_batch_size(self, buffer, db):
        """Test automatic flush when batch size reached."""
        event_id1 = generate_event_id()
        event_id2 = generate_event_id()
        
        buffer.add(event_id1, SignalType.WRONG_SKILL, {"test": 1})
        assert len(buffer._buffer) == 1
        
        buffer.add(event_id2, SignalType.SLOW_EXECUTION, {"test": 2})
        # Should auto-flush after 2nd record (batch_size=2)
        conn = db._mock_conn
        assert conn.execute.call_count == 3  # 2 INSERTs + 1 COMMIT
        assert len(buffer._buffer) == 0

    def test_manual_flush(self, buffer, db):
        """Test manual flush."""
        event_id = generate_event_id()
        buffer.add(event_id, SignalType.HIGH_COST, {"cost": 0.5})
        
        assert len(buffer._buffer) == 1
        flushed = buffer.flush()
        
        assert flushed == 1
        conn = db._mock_conn
        assert conn.execute.call_count == 2  # 1 INSERT + 1 COMMIT
        assert len(buffer._buffer) == 0

    def test_flush_empty_buffer(self, buffer, db):
        """Test flushing empty buffer."""
        flushed = buffer.flush()
        
        assert flushed == 0
        assert db.execute.call_count == 0

    def test_flush_error_requeues(self, buffer, db):
        """Test that flush errors re-queue signals."""
        conn = db._mock_conn
        conn.execute.side_effect = Exception("DB error")
        
        event_id = generate_event_id()
        buffer.add(event_id, SignalType.WRONG_SKILL, {"test": 1})
        buffer.add(event_id, SignalType.SLOW_EXECUTION, {"test": 2})
        
        # Auto-flush should fail and re-queue
        assert len(buffer._buffer) == 2

    def test_maybe_flush_interval_not_elapsed(self, buffer, db):
        """Test maybe_flush when interval not elapsed."""
        event_id = generate_event_id()
        buffer.add(event_id, SignalType.WRONG_SKILL, {"test": 1})
        
        flushed = buffer.maybe_flush()
        
        assert flushed == 0
        assert db.execute.call_count == 0
        assert len(buffer._buffer) == 1

    def test_signal_id_uniqueness(self, db):
        """Test that each signal gets unique ID."""
        # Use larger batch_size to prevent auto-flush
        buffer = _FeedbackBuffer(db, batch_size=10, flush_interval=60)
        event_id = generate_event_id()
        
        buffer.add(event_id, SignalType.WRONG_SKILL, {"test": 1})
        buffer.add(event_id, SignalType.WRONG_SKILL, {"test": 2})
        
        signal_ids = [s["signal_id"] for s in buffer._buffer]
        assert len(signal_ids) == 2
        assert signal_ids[0] != signal_ids[1]

    def test_all_signal_types(self, db):
        """Test all signal types."""
        # Use larger batch_size to prevent auto-flush
        buffer = _FeedbackBuffer(db, batch_size=10, flush_interval=60)
        event_id = generate_event_id()
        
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


class TestFeedbackBufferReliability:
    """Test _FeedbackBuffer overflow and retry reliability."""

    def test_buffer_overflow_drops_oldest(self, db):
        """Fill buffer beyond _MAX_BUFFER_SIZE, verify oldest signals dropped."""
        buf = _FeedbackBuffer(lambda: db, batch_size=20000, flush_interval=9999)
        # Inject signals directly to avoid auto-flush
        for i in range(buf._MAX_BUFFER_SIZE):
            buf._buffer.append({
                "signal_id": f"old_{i}",
                "selection_event_id": "evt",
                "signal_type": "wrong_skill",
                "signal_data": "{}",
                "created_at": datetime.now(timezone.utc),
            })
        assert len(buf._buffer) == buf._MAX_BUFFER_SIZE
        # Adding one more should trigger overflow
        buf.add("evt_new", SignalType.WRONG_SKILL, {"new": True})
        # Buffer should have dropped oldest batch_size signals
        assert len(buf._buffer) <= buf._MAX_BUFFER_SIZE
        ids = [s["signal_id"] for s in buf._buffer]
        assert "old_0" not in ids  # oldest dropped

    def test_buffer_overflow_preserves_newest(self, db):
        """After overflow, newest signal is retained."""
        buf = _FeedbackBuffer(lambda: db, batch_size=20000, flush_interval=9999)
        for i in range(buf._MAX_BUFFER_SIZE):
            buf._buffer.append({
                "signal_id": f"s_{i}",
                "selection_event_id": "evt",
                "signal_type": "wrong_skill",
                "signal_data": "{}",
                "created_at": datetime.now(timezone.utc),
            })
        buf.add("evt_newest", SignalType.HIGH_COST, {"newest": True})
        ids = [s["signal_id"] for s in buf._buffer]
        # The newest signal should be present
        assert any("evt_newest" in s["selection_event_id"] for s in buf._buffer)

    def test_retry_limit_drops_after_max_retries(self, db):
        """Signals dropped after _MAX_RETRIES flush failures."""
        conn = db._mock_conn
        conn.execute.side_effect = Exception("DB down")
        buf = _FeedbackBuffer(lambda: db, batch_size=1, flush_interval=9999)
        # Add a signal — auto-flush will fail
        buf.add("evt1", SignalType.WRONG_SKILL, {"x": 1})
        # After first failure, signal is re-queued with retry count 1
        assert len(buf._buffer) == 1
        # Flush again (retry 2)
        buf.flush()
        assert len(buf._buffer) == 1
        # Flush again (retry 3)
        buf.flush()
        assert len(buf._buffer) == 1
        # Flush again (retry 4 > MAX_RETRIES=3) — should be dropped
        buf.flush()
        assert len(buf._buffer) == 0

    def test_retry_counts_cleared_on_success(self, db):
        """Retry counts cleaned up after successful flush."""
        conn = db._mock_conn
        conn.execute.side_effect = Exception("DB down")
        buf = _FeedbackBuffer(lambda: db, batch_size=1, flush_interval=9999)
        buf.add("evt1", SignalType.WRONG_SKILL, {"x": 1})
        # First failure — retry count = 1
        assert len(buf._retry_counts) == 1
        # Fix DB
        conn.execute.side_effect = None
        conn.execute.reset_mock()
        buf.flush()
        # Should succeed and clear retry counts
        assert len(buf._buffer) == 0
        assert len(buf._retry_counts) == 0
