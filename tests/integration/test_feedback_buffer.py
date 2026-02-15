"""Integration tests for _FeedbackBuffer and skill_learning_signals table."""

import pytest
import time
import threading
import json
from datetime import datetime, timezone
from uuid_utils import uuid7
from sqlalchemy import text

from core.skills.pipeline import _FeedbackBuffer, SkillPipeline, SignalType
from core.skills.learning_signals import SignalWeights
from api.models import SkillLearningSignal, SkillSelectionEvent
from api.database import get_db_session


@pytest.fixture
def db():
    """Database session fixture."""
    return next(get_db_session())


@pytest.fixture
def clean_db(db):
    """Clean up test data after test."""
    yield
    # Clean up using ORM
    db.query(SkillLearningSignal).filter(SkillLearningSignal.signal_id.like("test_%")).delete(synchronize_session=False)
    db.query(SkillSelectionEvent).filter(SkillSelectionEvent.event_id.like("test_%")).delete(synchronize_session=False)
    db.commit()


class TestFeedbackBufferFlush:
    """Test batch flush functionality."""

    def test_feedback_buffer_flush_batch_size(self, db, clean_db):
        """Test that buffer flushes when batch size is reached."""
        buffer = _FeedbackBuffer(db, batch_size=3, flush_interval=3600)
        
        event_id = str(uuid7())
        
        # Add 3 signals - should auto-flush
        buffer.add(event_id, SignalType.WRONG_SKILL, {"reason": "test1"})
        buffer.add(event_id, SignalType.SLOW_EXECUTION, {"ms": 5000})
        buffer.add(event_id, SignalType.HIGH_COST, {"cost": 0.5})
        
        # Verify data in database
        result = db.execute(
            text("SELECT COUNT(*) FROM skill_learning_signals WHERE selection_event_id = :event_id"),
            {"event_id": event_id}
        ).fetchone()
        
        assert result[0] == 3

    def test_feedback_buffer_flush_manual(self, db, clean_db):
        """Test manual flush."""
        buffer = _FeedbackBuffer(db, batch_size=10, flush_interval=3600)
        
        event_id = str(uuid7())
        
        # Add 2 signals
        buffer.add(event_id, SignalType.WRONG_SKILL, {"reason": "test1"})
        buffer.add(event_id, SignalType.SLOW_EXECUTION, {"ms": 5000})
        
        # Verify not yet in DB
        result = db.execute(
            text("SELECT COUNT(*) FROM skill_learning_signals WHERE selection_event_id = :event_id"),
            {"event_id": event_id}
        ).fetchone()
        assert result[0] == 0
        
        # Manual flush
        flushed = buffer.flush()
        
        assert flushed == 2
        
        # Verify in DB
        result = db.execute(
            text("SELECT COUNT(*) FROM skill_learning_signals WHERE selection_event_id = :event_id"),
            {"event_id": event_id}
        ).fetchone()
        assert result[0] == 2

    def test_feedback_buffer_flush_empty(self, db, clean_db):
        """Test flushing empty buffer."""
        buffer = _FeedbackBuffer(db, batch_size=10, flush_interval=3600)
        
        flushed = buffer.flush()
        
        assert flushed == 0


class TestFeedbackBufferInterval:
    """Test time-based flush functionality."""

    def test_feedback_buffer_interval_not_elapsed(self, db, clean_db):
        """Test maybe_flush when interval not elapsed."""
        buffer = _FeedbackBuffer(db, batch_size=10, flush_interval=0.1)
        
        event_id = str(uuid7())
        buffer.add(event_id, SignalType.WRONG_SKILL, {"reason": "test"})
        
        # Immediately call maybe_flush - should not flush
        flushed = buffer.maybe_flush()
        
        assert flushed == 0
        
        # Verify not in DB
        result = db.execute(
            text("SELECT COUNT(*) FROM skill_learning_signals WHERE selection_event_id = :event_id"),
            {"event_id": event_id}
        ).fetchone()
        assert result[0] == 0

    def test_feedback_buffer_interval_elapsed(self, db, clean_db):
        """Test maybe_flush when interval elapsed."""
        buffer = _FeedbackBuffer(db, batch_size=10, flush_interval=0.05)
        
        event_id = str(uuid7())
        buffer.add(event_id, SignalType.WRONG_SKILL, {"reason": "test"})
        
        # Wait for interval to pass
        time.sleep(0.1)
        
        # Call maybe_flush - should flush
        flushed = buffer.maybe_flush()
        
        assert flushed == 1
        
        # Verify in DB
        result = db.execute(
            text("SELECT COUNT(*) FROM skill_learning_signals WHERE selection_event_id = :event_id"),
            {"event_id": event_id}
        ).fetchone()
        assert result[0] == 1


class TestFeedbackBufferConcurrent:
    """Test thread safety."""

    def test_feedback_buffer_concurrent(self, db, clean_db):
        """Test concurrent access from multiple threads."""
        buffer = _FeedbackBuffer(db, batch_size=100, flush_interval=3600)
        
        event_id = str(uuid7())
        num_threads = 5
        signals_per_thread = 20
        total_signals = num_threads * signals_per_thread
        
        def add_signals(thread_id):
            for i in range(signals_per_thread):
                buffer.add(
                    event_id,
                    SignalType.WRONG_SKILL,
                    {"thread": thread_id, "index": i}
                )
        
        # Create and start threads
        threads = []
        for i in range(num_threads):
            t = threading.Thread(target=add_signals, args=(i,))
            threads.append(t)
            t.start()
        
        # Wait for all threads to complete
        for t in threads:
            t.join()
        
        # Flush any remaining
        buffer.flush()
        
        # Verify all signals in DB
        result = db.execute(
            text("SELECT COUNT(*) FROM skill_learning_signals WHERE selection_event_id = :event_id"),
            {"event_id": event_id}
        ).fetchone()
        
        assert result[0] == total_signals


class TestSkillLearningSignalTable:
    """Test skill_learning_signals table exists and has correct schema."""

    def test_table_exists(self, db):
        """Test that skill_learning_signals table exists."""
        result = db.execute(
            text("""
                SELECT 1 FROM information_schema.tables 
                WHERE table_name = 'skill_learning_signals'
                """)
        ).fetchone()
        
        assert result is not None

    def test_table_columns(self, db):
        """Test that table has all required columns."""
        result = db.execute(
            text("""
                SELECT column_name, data_type, is_nullable
                FROM information_schema.columns
                WHERE table_name = 'skill_learning_signals'
                ORDER BY ordinal_position
                """)
        ).fetchall()
        
        columns = {row[0] for row in result}
        
        assert 'signal_id' in columns
        assert 'selection_event_id' in columns
        assert 'signal_type' in columns
        assert 'signal_data' in columns
        assert 'created_at' in columns

    def test_primary_key(self, db):
        """Test that signal_id is primary key."""
        # Check via SHOW CREATE TABLE
        result = db.execute(
            text("SHOW CREATE TABLE skill_learning_signals")
        ).fetchone()
        
        assert result is not None
        create_sql = result[1]
        assert 'PRIMARY KEY' in create_sql
        assert '`signal_id`' in create_sql

    def test_indexes(self, db):
        """Test that table has required indexes."""
        result = db.execute(
            text("""
                SELECT index_name, GROUP_CONCAT(column_name) as columns
                FROM information_schema.statistics
                WHERE table_name = 'skill_learning_signals'
                GROUP BY index_name
                """)
        ).fetchall()
        
        index_info = {row[0]: row[1] for row in result}
        
        # Check for indexes on common query columns
        has_selection_event_id = any('selection_event_id' in cols for cols in index_info.values())
        has_signal_type = any('signal_type' in cols for cols in index_info.values())
        has_created_at = any('created_at' in cols for cols in index_info.values())
        
        assert has_selection_event_id, "Should have index on selection_event_id"
        assert has_signal_type, "Should have index on signal_type"
        assert has_created_at, "Should have index on created_at"

    def test_insert_and_query(self, db, clean_db):
        """Test inserting and querying skill_learning_signals records using ORM."""
        event_id = str(uuid7())
        signal_id = str(uuid7())
        
        # Insert using ORM
        signal = SkillLearningSignal(
            signal_id=signal_id,
            selection_event_id=event_id,
            signal_type=SignalType.WRONG_SKILL.value,
            signal_data={"reason": "test"},
            created_at=datetime.now(timezone.utc).replace(tzinfo=None)
        )
        db.add(signal)
        db.commit()
        
        # Query back
        result = db.execute(
            text("SELECT * FROM skill_learning_signals WHERE signal_id = :signal_id"),
            {"signal_id": signal_id}
        ).fetchone()
        
        assert result is not None
        assert result[1] == event_id
        assert result[2] == SignalType.WRONG_SKILL.value
        assert json.loads(result[3]) == {"reason": "test"}


class TestSkillPipelineIntegration:
    """Test SkillPipeline with real database."""

    def test_skill_pipeline_record_feedback(self, db, clean_db):
        """Test SkillPipeline.record_feedback writes to database."""
        # Create a selection event first using ORM
        event_id = str(uuid7())
        session_id = str(uuid7())
        
        event = SkillSelectionEvent(
            event_id=event_id,
            session_id=session_id,
            user_query="test query",
            context_snapshot="test",
            available_skills=[],
            selected_skills=["test_skill"],
            selection_method="pipeline_v1",
            selection_reasoning="test",
            candidate_scores={},
            created_at=datetime.now(timezone.utc).replace(tzinfo=None)
        )
        db.add(event)
        db.commit()
        
        # Create pipeline with learning enabled to test feedback
        pipeline = SkillPipeline(
            db=db,
            llm_client=None,
            audit=False,  # Skip audit event creation
            learning=True  # Enable learning to record feedback
        )
        
        # Record feedback
        pipeline.record_feedback(event_id, SignalType.WRONG_SKILL, {"reason": "test"})
        
        # Flush feedback buffer
        flushed = pipeline.flush_feedback()
        
        assert flushed == 1
        
        # Verify in database
        result = db.execute(
            text("SELECT COUNT(*) FROM skill_learning_signals WHERE selection_event_id = :event_id"),
            {"event_id": event_id}
        ).fetchone()
        
        assert result[0] == 1

    def test_skill_pipeline_flush_on_close(self, db, clean_db):
        """Test that feedback is flushed when session closes."""
        event_id = str(uuid7())
        session_id = str(uuid7())
        
        # Create selection event using ORM
        event = SkillSelectionEvent(
            event_id=event_id,
            session_id=session_id,
            user_query="test query",
            context_snapshot="test",
            available_skills=[],
            selected_skills=["test_skill"],
            selection_method="pipeline_v1",
            selection_reasoning="test",
            candidate_scores={},
            created_at=datetime.now(timezone.utc).replace(tzinfo=None)
        )
        db.add(event)
        db.commit()
        
        # Create pipeline
        pipeline = SkillPipeline(
            db=db,
            llm_client=None,
            audit=False,
            learning=True,
            learning_weights=SignalWeights()
        )
        
        # Record multiple feedback items (below batch size)
        for i in range(3):
            pipeline.record_feedback(event_id, SignalType.WRONG_SKILL, {"index": i})
        
        # Verify not yet flushed (batch_size=50 default)
        result = db.execute(
            text("SELECT COUNT(*) FROM skill_learning_signals WHERE selection_event_id = :event_id"),
            {"event_id": event_id}
        ).fetchone()
        assert result[0] == 0
        
        # Flush on close
        flushed = pipeline.flush_feedback()
        
        assert flushed == 3
        
        # Verify in database
        result = db.execute(
            text("SELECT COUNT(*) FROM skill_learning_signals WHERE selection_event_id = :event_id"),
            {"event_id": event_id}
        ).fetchone()
        assert result[0] == 3
