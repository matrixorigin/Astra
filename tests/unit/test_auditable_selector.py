"""Minimal real DB tests for auditable selector."""

import json
from datetime import datetime, timezone
from unittest.mock import Mock
import uuid

import pytest

from core.skills.auditable_selector import AuditableSkillSelector, SkillSelectionEvent
from sdk import Database


@pytest.fixture
def db():
    """Real database."""
    database = Database()
    db_name = f"test_{uuid.uuid4().hex[:8]}"
    database.execute(f"CREATE DATABASE IF NOT EXISTS {db_name}")
    database.execute(f"USE {db_name}")
    database.database = db_name
    
    yield database
    
    database.execute(f"DROP DATABASE IF EXISTS {db_name}")


@pytest.fixture
def selector(db):
    """Auditable selector."""
    llm = Mock()
    llm.chat_with_tools = Mock(return_value={"tool_calls": []})
    sel = AuditableSkillSelector(db, llm)
    sel._ensure_table()
    return sel


class TestAuditableSkillSelector:
    """Core functionality tests."""

    def test_save_and_retrieve_event(self, selector, db):
        """Test event persistence."""
        event_id = f"evt-{uuid.uuid4().hex[:8]}"
        event = SkillSelectionEvent(
            event_id=event_id,
            session_id="sess-1",
            user_query="Test query",
            context_snapshot="snap",
            available_skills=[],
            selected_skills=["skill1"],
            selection_method="llm",
            selection_reasoning="Test",
            candidate_scores={},
        )
        
        selector._save_event(event)
        
        rows = db.fetchall(
            "SELECT * FROM skill_selection_events WHERE event_id = %s",
            (event_id,)
        )
        assert len(rows) == 1
        assert rows[0]["user_query"] == "Test query"

    def test_update_execution_result(self, selector, db):
        """Test updating execution result."""
        event_id = f"evt-{uuid.uuid4().hex[:8]}"
        event = SkillSelectionEvent(
            event_id=event_id,
            session_id="sess-1",
            user_query="Test",
            context_snapshot="snap",
            available_skills=[],
            selected_skills=["skill1"],
            selection_method="llm",
            selection_reasoning="Test",
            candidate_scores={},
        )
        selector._save_event(event)
        
        selector.update_execution_result(
            event_id=event_id,
            success=True,
            time_ms=1500,
            cost=0.05,
            result={"status": "ok"},
        )
        
        rows = db.fetchall(
            "SELECT * FROM skill_selection_events WHERE event_id = %s",
            (event_id,)
        )
        # MySQL may return 1, True, or 'true' depending on driver
        assert rows[0]["execution_success"] in (1, True, 'true')
        assert rows[0]["execution_time_ms"] == 1500

    def test_update_user_feedback(self, selector, db):
        """Test user feedback."""
        event_id = f"evt-{uuid.uuid4().hex[:8]}"
        event = SkillSelectionEvent(
            event_id=event_id,
            session_id="sess-1",
            user_query="Test",
            context_snapshot="snap",
            available_skills=[],
            selected_skills=["skill1"],
            selection_method="llm",
            selection_reasoning="Test",
            candidate_scores={},
        )
        selector._save_event(event)
        
        selector.update_user_feedback(event_id=event_id, score=5)
        
        rows = db.fetchall(
            "SELECT * FROM skill_selection_events WHERE event_id = %s",
            (event_id,)
        )
        assert rows[0]["user_feedback_score"] == 5

    def test_get_selection_history(self, selector):
        """Test history retrieval."""
        session_id = f"sess-{uuid.uuid4().hex[:8]}"
        
        for i in range(3):
            event = SkillSelectionEvent(
                event_id=f"evt-{i}-{uuid.uuid4().hex[:8]}",
                session_id=session_id,
                user_query=f"Query {i}",
                context_snapshot="snap",
                available_skills=[],
                selected_skills=["skill1"],
                selection_method="llm",
                selection_reasoning="Test",
                candidate_scores={},
            )
            selector._save_event(event)
        
        history = selector.get_selection_history(session_id=session_id)
        
        assert len(history) == 3

    def test_invalid_feedback_score(self, selector):
        """Test validation."""
        with pytest.raises(ValueError):
            selector.update_user_feedback(event_id="test", score=6)
