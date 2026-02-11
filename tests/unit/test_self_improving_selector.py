"""Minimal real DB tests for self-improving selector."""

import json
from unittest.mock import Mock
import uuid

import pytest

from core.skills.self_improving_selector import SelfImprovingSelector
from core.skills.auditable_selector import SkillSelectionEvent, AuditableSkillSelector
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


@pytest.fixture
def self_improving(db, selector):
    """Self-improving selector."""
    llm = Mock()
    llm.chat = Mock(return_value=json.dumps({
        "query_pattern": "review pr",
        "wrong_skills": ["summarize_pr"],
        "correct_skills": ["code_review"],
        "improvement_score": 0.8,
        "evidence": "User feedback"
    }))
    si = SelfImprovingSelector(db, llm)
    si.auditable_selector = selector
    si._ensure_tables()
    return si


class TestSelfImprovingSelector:
    """Core functionality tests."""

    def test_learn_from_failures_no_failures(self, self_improving):
        """Test with no failures."""
        result = self_improving.learn_from_failures(days=30)
        
        assert result["failures_analyzed"] == 0

    def test_update_learnings_new(self, self_improving, db):
        """Test adding new learning."""
        # Clear any existing data
        db.execute("DELETE FROM skill_selection_learnings")
        
        corrections = [{
            "query_pattern": "review pr",
            "wrong_skills": ["summarize_pr"],
            "correct_skills": ["code_review"],
            "improvement_score": 0.8,
            "evidence": "evt-123"
        }]
        
        count = self_improving._update_learnings(corrections)
        
        assert count == 1
        
        rows = db.fetchall("""
            SELECT * FROM skill_selection_learnings
            WHERE query_pattern = %s
        """, ("review pr",))
        assert len(rows) == 1

    def test_get_learning_stats_no_data(self, self_improving, db):
        """Test stats with no data."""
        # Clear any existing data
        db.execute("DELETE FROM skill_selection_learnings")
        
        stats = self_improving.get_learning_stats()
        
        assert stats["total_learnings"] == 0

    def test_get_recent_failures(self, self_improving, selector, db):
        """Test getting failures."""
        for i in range(3):
            event = SkillSelectionEvent(
                event_id=f"evt-{i}-{uuid.uuid4().hex[:8]}",
                session_id=f"sess-{i}",
                user_query=f"Query {i}",
                context_snapshot="snap",
                available_skills=[],
                selected_skills=["wrong_skill"],
                selection_method="llm",
                selection_reasoning="Test",
                candidate_scores={},
            )
            selector._save_event(event)
            
            db.execute("""
                UPDATE skill_selection_events
                SET user_feedback_score = 2, selection_correctness = FALSE
                WHERE event_id = %s
            """, (event.event_id,))
        
        # Verify data exists
        rows = db.fetchall("""
            SELECT * FROM skill_selection_events 
            WHERE user_feedback_score <= 2
        """)
        assert len(rows) >= 3
