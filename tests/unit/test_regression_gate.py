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

    def test_validate_no_golden_queries(self, gate, selector, db):
        """Test with no golden queries."""
        # Clear any existing golden queries
        db.execute("DELETE FROM skill_selection_events WHERE user_feedback_score >= 4")
        
        result = gate.validate_selector_change(
            new_selector=selector,
            old_selector=selector,
            selector_version="v1.0.0",
        )
        
        assert result["verdict"] == "SKIP"

    def test_get_gate_stats_no_data(self, gate, db):
        """Test stats with no data."""
        # Clear any existing data
        db.execute("DELETE FROM selector_gate_results")
        
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

    def test_test_selector(self, gate, selector):
        """Test _test_selector method."""
        queries = [
            SkillSelectionEvent(
                event_id="evt-1",
                session_id="sess-1",
                user_query="Test query",
                context_snapshot="snap",
                available_skills=[],
                selected_skills=["skill1"],
                selection_method="llm",
                selection_reasoning="Test",
                candidate_scores={},
            )
        ]
        
        with patch.object(selector, 'select_with_validation') as mock_select:
            mock_select.return_value = SkillSelectionEvent(
                event_id="new-evt",
                session_id="sess-1",
                user_query="Test",
                context_snapshot="snap",
                available_skills=[],
                selected_skills=["skill1"],
                selection_method="llm",
                selection_reasoning="Test",
                candidate_scores={"skill1": 0.9},
            )
            
            results = gate._test_selector(selector, queries, "sandbox")
            
            assert len(results) == 1
            assert "score" in results[0]

    def test_validate_creates_and_cleans_sandbox(self, gate, selector, db):
        """Test sandbox lifecycle during validation."""
        event = SkillSelectionEvent(
            event_id=f"evt-{uuid.uuid4().hex[:8]}",
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
        
        db.execute("""
            UPDATE skill_selection_events
            SET user_feedback_score = 5, selection_correctness = TRUE
            WHERE event_id = %s
        """, (event.event_id,))
        
        # Run validation
        gate.validate_selector_change(
            new_selector=selector,
            old_selector=selector,
            selector_version="v1.0.0",
        )
        
        # Verify no leftover sandboxes
        rows = db.fetchall("SHOW DATABASES")
        db_names = [row[list(row.keys())[0]] for row in rows]
        gate_dbs = [name for name in db_names if "gate_" in name]
        assert len(gate_dbs) == 0

    def test_get_gate_stats_with_results(self, gate, db):
        """Test stats calculation."""
        # Clear and insert some results directly
        db.execute("DELETE FROM selector_gate_results")
        
        for i in range(5):
            gate_id = f"gate-{i}-{uuid.uuid4().hex[:8]}"
            verdict = "PASS" if i < 3 else "FAIL"
            db.execute("""
                INSERT INTO selector_gate_results
                (gate_id, selector_version, test_queries_count,
                 new_selector_avg_score, old_selector_avg_score,
                 improvement_pct, verdict, details)
                VALUES (%s, %s, %s, %s, %s, %s, %s, %s)
            """, (
                gate_id, f"v1.{i}.0", 10,
                0.9 if verdict == "PASS" else 0.5,
                0.8, 10.0 if verdict == "PASS" else -20.0,
                verdict, json.dumps({})
            ))
        
        stats = gate.get_gate_stats()
        
        assert stats["total_gates"] == 5
        assert stats["passed"] == 3
        assert stats["failed"] == 2

    def test_validate_with_min_improvement(self, gate, selector, db):
        """Test validation with minimum improvement threshold."""
        event = SkillSelectionEvent(
            event_id=f"evt-{uuid.uuid4().hex[:8]}",
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
        
        db.execute("""
            UPDATE skill_selection_events
            SET user_feedback_score = 5, selection_correctness = TRUE
            WHERE event_id = %s
        """, (event.event_id,))
        
        result = gate.validate_selector_change(
            new_selector=selector,
            old_selector=selector,
            selector_version="v1.0.0",
            min_improvement=-0.1  # Allow 10% regression
        )
        
        assert "verdict" in result
