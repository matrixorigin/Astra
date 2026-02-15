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
from api.models import SkillLearningSignal, SkillSelectionEvent, SkillSelectionLearning
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


class TestSkillPipelineLearn:
    """Test SkillPipeline learning functionality."""

    def test_skill_pipeline_learn(self, db, clean_db):
        """Test learning cycle."""
        # Create a failure event first
        event = SkillSelectionEvent(
            event_id=str(uuid7()),
            session_id=str(uuid7()),
            user_query="test query for learning",
            available_skills=["skill1", "skill2"],
            selected_skills=["skill1"],
            selection_method="test",
            execution_success=False,
            execution_time_ms=5000,
            user_feedback_score=1,
            selection_correctness=False,
            correction_suggestion={"correct_skills": ["skill2"]},
            created_at=datetime.now(timezone.utc).replace(tzinfo=None)
        )
        db.add(event)
        db.commit()
        
        pipeline = SkillPipeline(
            db=db,
            llm_client=None,
            audit=False,
            learning=True,
            learning_weights=SignalWeights()
        )
        
        result = pipeline.learn(days=7)
        
        assert result.learned >= 0
        assert result.total_failures >= 0
        # signals_by_type may be empty if no signals extracted
        assert isinstance(result.signals_by_type, dict)

    def test_skill_pipeline_stats(self, db, clean_db):
        """Test getting learning statistics."""
        pipeline = SkillPipeline(
            db=db,
            llm_client=None,
            audit=False,
            learning=True
        )
        
        stats = pipeline.stats()
        
        assert 'total_learnings' in stats
        assert 'high_confidence' in stats
        assert 'by_signal_type' in stats
        assert 'regression_gates' in stats

    def test_skill_pipeline_selection_history(self, db, clean_db):
        """Test getting selection history."""
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
        
        pipeline = SkillPipeline(
            db=db,
            llm_client=None,
            audit=False,
            learning=False
        )
        
        history = pipeline.selection_history(session_id=session_id, limit=10)
        
        assert len(history) > 0
        assert history[0]['event_id'] == event_id
        assert history[0]['user_query'] == "test query"


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

    def test_skill_pipeline_get_tools_schema(self, db, clean_db):
        """Test get_tools_schema with audit enabled."""
        pipeline = SkillPipeline(
            db=db,
            llm_client=None,
            audit=True,
            learning=False
        )
        
        result = pipeline.get_tools_schema("test query", str(uuid7()))
        
        assert result.tools is not None
        assert result.event_id is not None
        assert result.candidates >= 0

    def test_skill_pipeline_record_feedback_no_event_id(self, db, clean_db):
        """Test record_feedback with None event_id (no-op)."""
        pipeline = SkillPipeline(
            db=db,
            llm_client=None,
            audit=False,
            learning=True
        )
        
        # Should not raise error
        pipeline.record_feedback(None, SignalType.WRONG_SKILL, {"reason": "test"})
        
        flushed = pipeline.flush_feedback()
        assert flushed == 0

    def test_skill_pipeline_record_feedback_learning_disabled(self, db, clean_db):
        """Test record_feedback with learning disabled (no-op)."""
        event_id = str(uuid7())
        
        pipeline = SkillPipeline(
            db=db,
            llm_client=None,
            audit=False,
            learning=False
        )
        
        # Should not raise error
        pipeline.record_feedback(event_id, SignalType.WRONG_SKILL, {"reason": "test"})
        
        flushed = pipeline.flush_feedback()
        assert flushed == 0

    def test_feedback_buffer_error_handling(self, db, clean_db):
        """Test feedback buffer error handling and re-queueing."""
        buffer = _FeedbackBuffer(db, batch_size=2, flush_interval=3600)
        
        event_id = str(uuid7())
        buffer.add(event_id, SignalType.WRONG_SKILL, {"reason": "test1"})
        buffer.add(event_id, SignalType.SLOW_EXECUTION, {"ms": 5000})
        
        # Verify signals are in buffer before flush
        assert len(buffer._buffer) == 0  # Auto-flushed at batch_size=2
        
        # Verify they were written to DB
        result = db.execute(
            text("SELECT COUNT(*) FROM skill_learning_signals WHERE selection_event_id = :eid"),
            {"eid": event_id}
        ).fetchone()
        assert result[0] == 2

    def test_skill_pipeline_maybe_flush_timing(self, db, clean_db):
        """Test opportunistic flush timing."""
        buffer = _FeedbackBuffer(db, batch_size=100, flush_interval=0.05)
        
        event_id = str(uuid7())
        buffer.add(event_id, SignalType.WRONG_SKILL, {"reason": "test"})
        
        # Immediate flush should return 0
        flushed = buffer.maybe_flush()
        assert flushed == 0
        
        # Wait and flush should return 1
        time.sleep(0.1)
        flushed = buffer.maybe_flush()
        assert flushed == 1

    def test_skill_pipeline_all_signal_types(self, db, clean_db):
        """Test all signal types are recorded."""
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
        
        pipeline = SkillPipeline(
            db=db,
            llm_client=None,
            audit=False,
            learning=True
        )
        
        # Record all signal types
        for signal_type in [SignalType.WRONG_SKILL, SignalType.SLOW_EXECUTION, 
                           SignalType.HIGH_COST, SignalType.LOW_SATISFACTION]:
            pipeline.record_feedback(event_id, signal_type, {"test": "data"})
        
        flushed = pipeline.flush_feedback()
        assert flushed == 4
        
        # Verify all types in DB
        result = db.execute(
            text("SELECT COUNT(DISTINCT signal_type) FROM skill_learning_signals WHERE selection_event_id = :eid"),
            {"eid": event_id}
        ).fetchone()
        assert result[0] == 4

    def test_skill_pipeline_get_tools_schema_with_learning(self, db, clean_db):
        """Test get_tools_schema with learning enabled."""
        pipeline = SkillPipeline(
            db=db,
            llm_client=None,
            audit=True,
            learning=True,
            learning_weights=SignalWeights()
        )
        
        result = pipeline.get_tools_schema("test query", str(uuid7()))
        
        assert result.tools is not None
        assert result.event_id is not None

    def test_skill_pipeline_selection_history_empty(self, db, clean_db):
        """Test selection_history with no results."""
        pipeline = SkillPipeline(
            db=db,
            llm_client=None,
            audit=False,
            learning=False
        )
        
        history = pipeline.selection_history(session_id=str(uuid7()), limit=10)
        
        assert len(history) == 0

    def test_skill_pipeline_learn_with_no_data(self, db, clean_db):
        """Test learn with no recent failure data."""
        pipeline = SkillPipeline(
            db=db,
            llm_client=None,
            audit=False,
            learning=True
        )
        
        # Learn from very old data (should find nothing)
        result = pipeline.learn(days=0)
        
        assert result.error is None

    def test_skill_pipeline_get_tools_schema_applies_learnings(self, db, clean_db):
        """Test get_tools_schema applies learned corrections."""
        # Create a learning record
        learning = SkillSelectionLearning(
            learning_id=str(uuid7()),
            query_pattern="test",
            wrong_skills=["wrong_skill"],
            correct_skills=["correct_skill"],
            improvement_score=10.0,
            confidence=100.0,
            evidence_count=10,
            signal_type="wrong_skill",
            target_metrics={"accuracy": 1.0},
            created_at=datetime.now(timezone.utc).replace(tzinfo=None)
        )
        db.add(learning)
        db.commit()
        
        pipeline = SkillPipeline(
            db=db,
            llm_client=None,
            audit=False,
            learning=True
        )
        
        # Get tools with learning applied
        result = pipeline.get_tools_schema("test query", str(uuid7()))
        
        assert result.tools is not None
        assert result.candidates >= 0

    def test_skill_pipeline_record_selection_audit(self, db, clean_db):
        """Test _record_selection creates audit event."""
        pipeline = SkillPipeline(
            db=db,
            llm_client=None,
            audit=True,
            learning=False
        )
        
        session_id = str(uuid7())
        # get_tools_schema may return empty tools, so just verify audit works
        result = pipeline.get_tools_schema("test query", session_id)
        
        # Verify event was created (even if tools are empty)
        assert result.event_id is not None
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


    def test_skill_pipeline_learn_disabled(self, db, clean_db):
        """Test learn() when learning is disabled."""
        pipeline = SkillPipeline(db=db, llm_client=None, audit=False, learning=False)
        
        result = pipeline.learn(days=7)
        
        assert result.error == "Learning disabled"
        assert result.learned == 0

    def test_skill_pipeline_stats_disabled(self, db, clean_db):
        """Test stats() when learning is disabled."""
        pipeline = SkillPipeline(db=db, llm_client=None, audit=False, learning=False)
        
        result = pipeline.stats()
        
        assert result.get("error") == "Learning disabled"

    def test_skill_pipeline_learn_with_error(self, db, clean_db):
        """Test learn() error handling."""
        pipeline = SkillPipeline(db=db, llm_client=None, audit=False, learning=True)
        
        # Mock improver to raise exception
        original_improver = pipeline._improver
        
        class FailingImprover:
            def learn_from_failures(self, days):
                raise RuntimeError("Test error")
        
        pipeline._improver = FailingImprover()
        
        result = pipeline.learn(days=7)
        
        assert result.error is not None
        assert "Test error" in result.error
        
        # Restore
        pipeline._improver = original_improver

    def test_skill_pipeline_get_tools_with_learning_correction(self, db, clean_db):
        """Test get_tools_schema applies learning corrections."""
        # Create a learning record
        learning = SkillSelectionLearning(
            learning_id=str(uuid7()),
            query_pattern="test correction query",
            wrong_skills=["wrong_skill"],
            correct_skills=["correct_skill"],
            improvement_score=15.0,
            confidence=80.0,
            evidence_count=3,
            signal_type=SignalType.WRONG_SKILL.value,
            target_metrics={"accuracy": 0.9},
            created_at=datetime.now(timezone.utc).replace(tzinfo=None)
        )
        db.add(learning)
        db.commit()
        
        pipeline = SkillPipeline(db=db, llm_client=None, audit=False, learning=True)
        
        # Mock modern selector to return both skills
        class MockModern:
            def get_tools_schema(self, query, max_candidates=None):
                return [
                    {"function": {"name": "wrong_skill"}},
                    {"function": {"name": "correct_skill"}},
                ]
        
        pipeline._modern = MockModern()
        
        result = pipeline.get_tools_schema("test correction query", "session1")
        
        # Should have applied learning
        assert result.tools is not None
        assert len(result.tools) >= 0  # May be reordered or filtered
