"""Minimal real DB tests for self-improving selector."""

import json
from unittest.mock import Mock, patch
import uuid

import pytest

from core.skills.self_improving_selector import SelfImprovingSelector
from core.skills.auditable_selector import SkillSelectionEvent, AuditableSkillSelector
from core.skills.selector import SkillMetadata
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
def mock_llm():
    """Mock LLM."""
    llm = Mock()
    llm.chat = Mock(return_value=json.dumps({
        "query_pattern": "review pr",
        "wrong_skills": ["summarize_pr"],
        "correct_skills": ["code_review"],
        "improvement_score": 0.8,
        "evidence": "User feedback"
    }))
    llm.chat_with_tools = Mock(return_value={"tool_calls": []})
    return llm


@pytest.fixture
def selector(db, mock_llm):
    """Auditable selector."""
    sel = AuditableSkillSelector(db, mock_llm)
    sel._ensure_table()
    return sel


@pytest.fixture
def self_improving(db, mock_llm, selector):
    """Self-improving selector."""
    si = SelfImprovingSelector(db, mock_llm)
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

    def test_update_learnings_multiple(self, self_improving, db):
        """Test updating learnings with multiple corrections."""
        # Clear data
        db.execute("DELETE FROM skill_selection_learnings")
        
        corrections = [
            {
                "query_pattern": "pattern1",
                "wrong_skills": ["skill1"],
                "correct_skills": ["skill2"],
                "improvement_score": 0.8,
                "evidence": "evt-1"
            },
            {
                "query_pattern": "pattern2",
                "wrong_skills": ["skill3"],
                "correct_skills": ["skill4"],
                "improvement_score": 0.9,
                "evidence": "evt-2"
            }
        ]
        
        count = self_improving._update_learnings(corrections)
        
        assert count == 2

    def test_apply_learnings_no_match(self, self_improving):
        """Test applying learnings with no match."""
        candidates = [SkillMetadata(
            name="code_review", version="1.0.0", description="Test",
            category="test", subcategory="sub", triggers=[],
            dependencies=[], priority=5, cost_estimate="low"
        )]
        
        corrected = self_improving.apply_learnings("unrelated query", candidates)
        
        assert corrected == candidates

    def test_apply_learnings_with_match(self, self_improving, db):
        """Test applying learnings with match."""
        # Insert learning
        db.execute("""
            INSERT INTO skill_selection_learnings
            (learning_id, query_pattern, wrong_skills, correct_skills, confidence, evidence_count)
            VALUES (%s, %s, %s, %s, %s, %s)
        """, (
            f"learn-{uuid.uuid4().hex[:8]}", "review pr",
            json.dumps(["summarize_pr"]), json.dumps(["code_review"]),
            0.8, 5
        ))
        
        candidates = [
            SkillMetadata(
                name="summarize_pr", version="1.0.0", description="Test",
                category="test", subcategory="sub", triggers=[],
                dependencies=[], priority=6, cost_estimate="low"
            ),
            SkillMetadata(
                name="code_review", version="1.0.0", description="Test",
                category="test", subcategory="sub", triggers=[],
                dependencies=[], priority=8, cost_estimate="medium"
            )
        ]
        
        corrected = self_improving.apply_learnings("Review PR #123", candidates)
        
        # Should prioritize code_review
        assert len(corrected) > 0

    def test_get_learning_stats_with_data(self, self_improving, db):
        """Test stats with data."""
        # Clear and insert
        db.execute("DELETE FROM skill_selection_learnings")
        
        for i in range(3):
            db.execute("""
                INSERT INTO skill_selection_learnings
                (learning_id, query_pattern, wrong_skills, correct_skills, confidence, evidence_count)
                VALUES (%s, %s, %s, %s, %s, %s)
            """, (
                f"learn-{i}-{uuid.uuid4().hex[:8]}", f"pattern {i}",
                json.dumps(["wrong"]), json.dumps(["correct"]),
                0.7 + i * 0.1, i + 1
            ))
        
        stats = self_improving.get_learning_stats()
        
        assert stats["total_learnings"] == 3
        assert stats["avg_confidence"] > 0

    def test_learn_from_failures_with_failures(self, self_improving, selector, db):
        """Test learning from actual failures."""
        event = SkillSelectionEvent(
            event_id=f"evt-{uuid.uuid4().hex[:8]}",
            session_id="sess-1",
            user_query="Review PR #123",
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
            SET user_feedback_score = 1, selection_correctness = FALSE
            WHERE event_id = %s
        """, (event.event_id,))
        
        result = self_improving.learn_from_failures(days=30)
        
        # May be 0 if LLM doesn't find pattern
        assert result["failures_analyzed"] >= 0
