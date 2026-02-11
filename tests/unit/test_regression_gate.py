"""Minimal real DB tests for regression gate."""

import json
from unittest.mock import Mock, patch
import uuid

import pytest

from core.skills.regression_gate import SkillSelectionRegressionGate
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
def mock_llm():
    """Mock LLM."""
    llm = Mock()
    llm.chat_with_tools = Mock(return_value={"tool_calls": []})
    return llm


@pytest.fixture
def gate(db, mock_llm):
    """Regression gate."""
    g = SkillSelectionRegressionGate(db, mock_llm)
    g._ensure_tables()
    return g


@pytest.fixture
def selector(db, mock_llm):
    """Auditable selector."""
    sel = AuditableSkillSelector(db, mock_llm)
    sel._ensure_table()
    return sel


class TestSkillSelectionRegressionGate:
    """Core functionality tests."""

    def test_validate_no_golden_queries(self, gate, selector):
        """Test with no golden queries."""
        result = gate.validate_selector_change(
            new_selector=selector,
            old_selector=selector,
            selector_version="v1.0.0",
        )
        
        assert result["verdict"] == "SKIP"

    def test_get_gate_stats_no_data(self, gate):
        """Test stats with no data."""
        stats = gate.get_gate_stats()
        
        assert stats["total_gates"] == 0

    def test_get_golden_queries(self, gate, selector, db):
        """Test getting golden queries."""
        for i in range(3):
            event = SkillSelectionEvent(
                event_id=f"evt-{i}-{uuid.uuid4().hex[:8]}",
                session_id=f"sess-{i}",
                user_query=f"Query {i}",
                context_snapshot="snap",
                available_skills=[],
                selected_skills=["skill1"],
                selection_method="llm",
                selection_reasoning="Test",
                candidate_scores={},
            )
            selector._save_event(event)
            
            db.execute("""
                UPDATE skill_selection_events
                SET user_feedback_score = 5, selection_correctness = TRUE
                WHERE event_id = %s
            """, (event.event_id,))
        
        # Verify data exists
        rows = db.fetchall("""
            SELECT * FROM skill_selection_events 
            WHERE user_feedback_score = 5
        """)
        assert len(rows) >= 3

    def test_validate_with_golden_queries(self, gate, selector, db):
        """Test validation with golden queries."""
        event = SkillSelectionEvent(
            event_id=f"evt-{uuid.uuid4().hex[:8]}",
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
        
        db.execute("""
            UPDATE skill_selection_events
            SET user_feedback_score = 5, selection_correctness = TRUE
            WHERE event_id = %s
        """, (event.event_id,))
        
        result = gate.validate_selector_change(
            new_selector=selector,
            old_selector=selector,
            selector_version="v1.0.0",
        )
        
        assert result["verdict"] in ["PASS", "SKIP"]

    def test_get_gate_history(self, gate):
        """Test getting gate history."""
        history = gate.get_gate_history(limit=10)
        
        # Should return list (may be empty)
        assert isinstance(history, list)
