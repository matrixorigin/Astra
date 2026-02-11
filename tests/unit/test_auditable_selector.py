"""Minimal real DB tests for auditable selector."""

import json
from datetime import datetime, timezone
from unittest.mock import Mock, patch
import uuid

import pytest

from core.skills.auditable_selector import AuditableSkillSelector, SkillSelectionEvent
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
    llm.chat_with_tools = Mock(return_value={"tool_calls": []})
    return llm


@pytest.fixture
def selector(db, mock_llm):
    """Auditable selector."""
    sel = AuditableSkillSelector(db, mock_llm)
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

    def test_select_with_validation_no_candidates(self, selector):
        """Test selection with no candidates."""
        with patch.object(selector, '_select_candidates', return_value=[]):
            event = selector.select_with_validation(
                query="xyz unrelated",
                session_id="sess-1",
                validate_in_sandbox=False
            )
            
            assert event.selected_skills == []
            assert event.selection_method == "none"

    def test_select_with_validation_single_candidate(self, selector):
        """Test selection with single candidate."""
        skill = SkillMetadata(
            name="test_skill", version="1.0.0", description="Test",
            category="test", subcategory="sub", triggers=["test"],
            dependencies=[], priority=5, cost_estimate="low"
        )
        
        with patch.object(selector, '_select_candidates', return_value=[skill]):
            event = selector.select_with_validation(
                query="test query",
                session_id="sess-1",
                validate_in_sandbox=False
            )
            
            assert len(event.selected_skills) > 0

    def test_create_selection_snapshot_fallback(self, selector):
        """Test snapshot creation fallback."""
        # Mock DB to raise error
        with patch.object(selector.db, 'execute', side_effect=Exception("DB error")):
            snapshot_id = selector._create_selection_snapshot("sess-1", "evt-1")
            
            # Should fallback to timestamp
            assert "snapshot_" in snapshot_id

    def test_get_available_skills_empty(self, selector, db):
        """Test getting skills when none exist."""
        # Mock empty result
        with patch.object(db, 'fetchall', return_value=[]):
            skills = selector._get_available_skills()
            
            assert skills == []

    def test_dry_run_skill(self, selector):
        """Test dry run simulation."""
        skill = SkillMetadata(
            name="test_skill", version="1.0.0", description="Test",
            category="test", subcategory="sub", triggers=[],
            dependencies=[], priority=8, cost_estimate="medium"
        )
        
        result = selector._dry_run_skill("sandbox", skill, "query", "snapshot")
        
        assert "success" in result
        assert "score" in result
        assert "time_ms" in result

    def test_update_feedback_low_score(self, selector, db):
        """Test feedback with low score marks as incorrect."""
        event_id = f"evt-{uuid.uuid4().hex[:8]}"
        event = SkillSelectionEvent(
            event_id=event_id,
            session_id="sess-1",
            user_query="Test",
            context_snapshot="snap",
            available_skills=[],
            selected_skills=["wrong_skill"],
            selection_method="llm",
            selection_reasoning="Test",
            candidate_scores={},
        )
        selector._save_event(event)
        
        selector.update_user_feedback(event_id=event_id, score=2)
        
        rows = db.fetchall(
            "SELECT * FROM skill_selection_events WHERE event_id = %s",
            (event_id,)
        )
        assert rows[0]["user_feedback_score"] == 2
        assert rows[0]["selection_correctness"] in (0, False, 'false')

    def test_validate_in_sandbox(self, selector):
        """Test sandbox validation."""
        skills = [
            SkillMetadata(
                name="skill1", version="1.0.0", description="Test",
                category="test", subcategory="sub", triggers=[],
                dependencies=[], priority=8, cost_estimate="low"
            ),
            SkillMetadata(
                name="skill2", version="1.0.0", description="Test",
                category="test", subcategory="sub", triggers=[],
                dependencies=[], priority=6, cost_estimate="medium"
            )
        ]
        
        result = selector._validate_in_sandbox(skills, "test query", "snapshot", "evt-1")
        
        assert "selected" in result
        assert "scores" in result
        assert "reasoning" in result
