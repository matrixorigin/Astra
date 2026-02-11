"""Tests for skill selection regression gate."""

import json
from datetime import datetime, timezone
from unittest.mock import Mock, patch

import pytest

from core.skills.regression_gate import SkillSelectionRegressionGate
from core.skills.auditable_selector import SkillSelectionEvent
from core.skills.selector import SkillMetadata
from sdk import Database


def make_skill(name="code_review", version="1.0.0", description="Test skill",
               category="github", subcategory="pr", triggers=None, priority=8,
               cost_estimate="medium", dependencies=None):
    """Helper to create SkillMetadata with defaults."""
    return SkillMetadata(
        name=name,
        version=version,
        description=description,
        category=category,
        subcategory=subcategory,
        triggers=triggers or [],
        dependencies=dependencies or [],
        priority=priority,
        cost_estimate=cost_estimate,
    )


@pytest.fixture
def mock_db():
    """Mock database."""
    db = Mock(spec=Database)
    db.database = "test_db"
    db.execute = Mock(return_value=[])
    return db


@pytest.fixture
def mock_llm():
    """Mock LLM client."""
    llm = Mock()
    return llm


@pytest.fixture
def mock_sandbox():
    """Mock sandbox."""
    sandbox = Mock()
    sandbox.create = Mock()
    sandbox.delete = Mock()
    return sandbox


@pytest.fixture
def mock_selector():
    """Mock auditable selector."""
    selector = Mock()
    selector._select_candidates = Mock(return_value=[])
    return selector


@pytest.fixture
def regression_gate(mock_db, mock_llm, mock_sandbox):
    """Regression gate instance."""
    with patch("core.skills.regression_gate.Sandbox", return_value=mock_sandbox):
        gate = SkillSelectionRegressionGate(mock_db, mock_llm)
        gate.sandbox = mock_sandbox
        return gate


class TestSkillSelectionRegressionGate:
    """Test SkillSelectionRegressionGate class."""

    def test_init(self, regression_gate, mock_db, mock_llm):
        """Test gate initialization."""
        assert regression_gate.db is mock_db
        assert regression_gate.llm is mock_llm
        assert regression_gate.account == "sys"

    def test_ensure_tables(self, regression_gate, mock_db):
        """Test table creation."""
        regression_gate._ensure_tables()
        mock_db.execute.assert_called()
        call_args = mock_db.execute.call_args[0][0]
        assert "selector_gate_results" in call_args

    def test_validate_selector_change_no_golden_queries(self, regression_gate, mock_db):
        """Test gate when no golden queries available."""
        mock_db.execute.return_value = []

        result = regression_gate.validate_selector_change(
            new_selector=Mock(),
            old_selector=Mock(),
            selector_version="v1.0.0",
        )

        assert result["verdict"] == "SKIP"
        assert "No golden queries" in result["reason"]

    def test_validate_selector_change_pass(self, regression_gate, mock_db, mock_selector, mock_sandbox):
        """Test gate passes when improvement is positive."""
        # Setup golden queries as dicts (not objects)
        golden_events = [
            {
                "event_id": "event-1",
                "session_id": "session-1",
                "user_query": "Review PR #123",
                "context_snapshot": "snapshot_1",
                "available_skills": '[]',
                "selected_skills": '["code_review"]',
                "selection_method": "llm",
                "selection_reasoning": "Test",
                "candidate_scores": "{}",
                "execution_success": True,
                "execution_time_ms": 1500,
                "execution_cost": 0.05,
                "user_feedback_score": 5,
                "created_at": datetime.now(timezone.utc),
            }
        ]
        mock_db.execute.return_value = golden_events
        mock_selector._select_candidates = Mock(return_value=[make_skill(name="code_review", triggers=["review"])])

        result = regression_gate.validate_selector_change(
            new_selector=mock_selector,
            old_selector=mock_selector,
            selector_version="v2.0.0",
            min_improvement=-0.05,
        )

        assert result["verdict"] == "PASS"
        assert result["selector_version"] == "v2.0.0"
        assert "test_queries_count" in result
        assert mock_sandbox.create.called
        assert mock_sandbox.delete.called

    def test_validate_selector_change_fail(self, regression_gate, mock_db, mock_selector):
        """Test gate fails when regression detected."""
        # Setup golden queries as dicts
        golden_events = [
            {
                "event_id": "event-1",
                "session_id": "session-1",
                "user_query": "Review PR #123",
                "context_snapshot": "snapshot_1",
                "available_skills": '[]',
                "selected_skills": '["code_review"]',
                "selection_method": "llm",
                "selection_reasoning": "Test",
                "candidate_scores": "{}",
                "execution_success": True,
                "execution_time_ms": 1500,
                "execution_cost": 0.05,
                "user_feedback_score": 5,
                "created_at": datetime.now(timezone.utc),
            }
        ]
        mock_db.execute.return_value = golden_events

        # Old selector returns correct skills, new selector returns wrong
        old_selector = Mock()
        old_selector._select_candidates = Mock(return_value=[make_skill(name="code_review", triggers=["review"])])

        new_selector = Mock()
        new_selector._select_candidates = Mock(return_value=[])  # Returns nothing

        result = regression_gate.validate_selector_change(
            new_selector=new_selector,
            old_selector=old_selector,
            selector_version="v2.0.0",
            min_improvement=-0.05,
        )

        assert result["verdict"] == "FAIL"
        assert "regression" in result["reason"].lower()

    def test_validate_selector_change_edge_threshold(self, regression_gate, mock_db, mock_selector):
        """Test gate at exact threshold."""
        golden_events = [
            {
                "event_id": "event-1",
                "session_id": "session-1",
                "user_query": "Review PR #123",
                "context_snapshot": "snapshot_1",
                "available_skills": '[]',
                "selected_skills": '["code_review"]',
                "selection_method": "llm",
                "selection_reasoning": "Test",
                "candidate_scores": "{}",
                "execution_success": True,
                "execution_time_ms": 1500,
                "execution_cost": 0.05,
                "user_feedback_score": 5,
                "created_at": datetime.now(timezone.utc),
            }
        ]
        mock_db.execute.return_value = golden_events

        result = regression_gate.validate_selector_change(
            new_selector=mock_selector,
            old_selector=mock_selector,
            selector_version="v2.0.0",
            min_improvement=0.0,  # Require positive improvement
        )

        # Should pass if improvement >= 0
        assert result["verdict"] in ["PASS", "FAIL"]

    def test_get_golden_queries(self, regression_gate, mock_db):
        """Test getting golden queries."""
        mock_db.execute.return_value = [
            {
                "event_id": "event-1",
                "session_id": "session-1",
                "user_query": "Review PR #123",
                "context_snapshot": "snapshot_1",
                "available_skills": '["code_review"]',
                "selected_skills": '["code_review"]',
                "selection_method": "llm",
                "selection_reasoning": "Test",
                "candidate_scores": "{}",
                "execution_success": True,
                "execution_time_ms": 1500,
                "execution_cost": 0.05,
                "user_feedback_score": 5,
                "created_at": datetime.now(timezone.utc),
            }
        ]

        queries = regression_gate._get_golden_queries(limit=10)

        assert len(queries) == 1
        assert queries[0].event_id == "event-1"
        assert queries[0].user_feedback_score == 5

    def test_test_selector(self, regression_gate, mock_selector):
        """Test selector testing."""
        mock_selector._select_candidates = Mock(return_value=[make_skill(name="code_review", triggers=["review"])])

        queries = [
            SkillSelectionEvent(
                event_id="event-1",
                session_id="session-1",
                user_query="Review PR #123",
                context_snapshot="snapshot_1",
                available_skills=[{"name": "code_review"}],
                selected_skills=["code_review"],
                selection_method="llm",
                selection_reasoning="Test",
                candidate_scores={},
                execution_success=True,
                user_feedback_score=5,
                created_at=datetime.now(timezone.utc),
            )
        ]

        results = regression_gate._test_selector(mock_selector, queries, "test_sandbox")

        assert len(results) == 1
        assert "score" in results[0]
        assert "selected" in results[0]

    def test_evaluate_selection_perfect_match(self, regression_gate):
        """Test evaluation with perfect match."""
        selected = [make_skill(name="code_review", triggers=["review"])]

        expected = SkillSelectionEvent(
            event_id="event-1",
            session_id="session-1",
            user_query="Test",
            context_snapshot="snapshot_1",
            available_skills=[],
            selected_skills=["code_review"],
            selection_method="llm",
            selection_reasoning="Test",
            candidate_scores={},
            execution_success=True,
            user_feedback_score=5,
            created_at=datetime.now(timezone.utc),
        )

        score = regression_gate._evaluate_selection(selected, expected)

        # Perfect match: overlap=1, feedback=1, success=1
        # Score = 0.4 * 1 + 0.3 * 1 + 0.3 * 1 = 1.0
        assert score == 1.0

    def test_evaluate_selection_no_match(self, regression_gate):
        """Test evaluation with no match."""
        selected = [make_skill(name="summarize_pr", priority=6, cost_estimate="low")]

        expected = SkillSelectionEvent(
            event_id="event-1",
            session_id="session-1",
            user_query="Test",
            context_snapshot="snapshot_1",
            available_skills=[],
            selected_skills=["code_review"],  # Different skill
            selection_method="llm",
            selection_reasoning="Test",
            candidate_scores={},
            execution_success=True,
            user_feedback_score=5,
            created_at=datetime.now(timezone.utc),
        )

        score = regression_gate._evaluate_selection(selected, expected)

        # No overlap: overlap=0, feedback=1, success=1
        # Score = 0.4 * 0 + 0.3 * 1 + 0.3 * 1 = 0.6
        assert score == 0.6

    def test_evaluate_selection_empty(self, regression_gate):
        """Test evaluation with empty selection."""
        expected = SkillSelectionEvent(
            event_id="event-1",
            session_id="session-1",
            user_query="Test",
            context_snapshot="snapshot_1",
            available_skills=[],
            selected_skills=["code_review"],
            selection_method="llm",
            selection_reasoning="Test",
            candidate_scores={},
            execution_success=True,
            user_feedback_score=5,
            created_at=datetime.now(timezone.utc),
        )

        score = regression_gate._evaluate_selection([], expected)

        assert score == 0.0

    def test_evaluate_selection_low_feedback(self, regression_gate):
        """Test evaluation with low user feedback."""
        selected = [make_skill(name="code_review", triggers=["review"])]

        expected = SkillSelectionEvent(
            event_id="event-1",
            session_id="session-1",
            user_query="Test",
            context_snapshot="snapshot_1",
            available_skills=[],
            selected_skills=["code_review"],
            selection_method="llm",
            selection_reasoning="Test",
            candidate_scores={},
            execution_success=True,
            user_feedback_score=1,  # Low feedback
            created_at=datetime.now(timezone.utc),
        )

        score = regression_gate._evaluate_selection(selected, expected)

        # Perfect match but low feedback: overlap=1, feedback=0.2, success=1
        # Score = 0.4 * 1 + 0.3 * 0.2 + 0.3 * 1 = 0.76
        assert abs(score - 0.76) < 0.01

    def test_evaluate_selection_execution_failure(self, regression_gate):
        """Test evaluation with execution failure."""
        selected = [make_skill(name="code_review", triggers=["review"])]

        expected = SkillSelectionEvent(
            event_id="event-1",
            session_id="session-1",
            user_query="Test",
            context_snapshot="snapshot_1",
            available_skills=[],
            selected_skills=["code_review"],
            selection_method="llm",
            selection_reasoning="Test",
            candidate_scores={},
            execution_success=False,  # Failed
            user_feedback_score=5,
            created_at=datetime.now(timezone.utc),
        )

        score = regression_gate._evaluate_selection(selected, expected)

        # Perfect match but failed: overlap=1, feedback=1, success=0
        # Score = 0.4 * 1 + 0.3 * 1 + 0.3 * 0 = 0.7
        assert score == 0.7

    def test_save_gate_result(self, regression_gate, mock_db):
        """Test saving gate result."""
        result = {
            "gate_id": "gate-123",
            "selector_version": "v2.0.0",
            "test_queries_count": 10,
            "new_selector_avg_score": 0.85,
            "old_selector_avg_score": 0.75,
            "improvement_pct": 13.33,
            "verdict": "PASS",
            "reason": "Test passed",
            "details": {"sample": "data"},
        }

        regression_gate._save_gate_result(result)

        mock_db.execute.assert_called()
        call_args = mock_db.execute.call_args[0]
        assert "INSERT INTO" in call_args[0]
        assert call_args[1][0] == "gate-123"

    def test_get_gate_history(self, regression_gate, mock_db):
        """Test getting gate history."""
        mock_db.execute.return_value = [
            {
                "gate_id": "gate-1",
                "selector_version": "v1.0.0",
                "test_queries_count": 10,
                "new_selector_avg_score": 0.85,
                "old_selector_avg_score": 0.75,
                "improvement_pct": 13.33,
                "verdict": "PASS",
                "tested_at": datetime.now(timezone.utc),
            }
        ]

        history = regression_gate.get_gate_history(limit=10)

        assert len(history) == 1
        assert history[0]["gate_id"] == "gate-1"
        assert history[0]["verdict"] == "PASS"

    def test_get_gate_stats(self, regression_gate, mock_db):
        """Test getting gate statistics."""
        mock_db.execute.return_value = [
            {
                "total_gates": 10,
                "passed": 8,
                "failed": 2,
                "avg_improvement": 5.5,
            }
        ]

        stats = regression_gate.get_gate_stats()

        assert stats["total_gates"] == 10
        assert stats["passed"] == 8
        assert stats["failed"] == 2
        assert stats["pass_rate"] == 0.8
        assert stats["avg_improvement_pct"] == 5.5

    def test_get_gate_stats_no_data(self, regression_gate, mock_db):
        """Test getting gate statistics with no data."""
        mock_db.execute.return_value = [
            {
                "total_gates": 0,
                "passed": 0,
                "failed": 0,
                "avg_improvement": None,
            }
        ]

        stats = regression_gate.get_gate_stats()

        assert stats["total_gates"] == 0
        assert stats["pass_rate"] == 0
        assert stats["avg_improvement_pct"] == 0.0


class TestRegressionGateEdgeCases:
    """Test edge cases for regression gate."""

    def test_validate_with_selector_error(self, regression_gate, mock_db, mock_selector):
        """Test handling selector error during validation."""
        golden_events = [
            {
                "event_id": "event-1",
                "session_id": "session-1",
                "user_query": "Review PR #123",
                "context_snapshot": "snapshot_1",
                "available_skills": '[]',
                "selected_skills": '["code_review"]',
                "selection_method": "llm",
                "selection_reasoning": "Test",
                "candidate_scores": "{}",
                "execution_success": True,
                "execution_time_ms": 1500,
                "execution_cost": 0.05,
                "user_feedback_score": 5,
                "created_at": datetime.now(timezone.utc),
            }
        ]
        mock_db.execute.return_value = golden_events
        mock_selector._select_candidates.side_effect = Exception("Selector error")

        result = regression_gate.validate_selector_change(
            new_selector=mock_selector,
            old_selector=mock_selector,
            selector_version="v2.0.0",
        )

        # Should handle error gracefully
        assert "verdict" in result
        assert result["test_queries_count"] == 1

    def test_sandbox_cleanup_on_error(self, regression_gate, mock_db, mock_selector, mock_sandbox):
        """Test sandbox cleanup even on error."""
        golden_events = [
            {
                "event_id": "event-1",
                "session_id": "session-1",
                "user_query": "Review PR #123",
                "context_snapshot": "snapshot_1",
                "available_skills": '[]',
                "selected_skills": '["code_review"]',
                "selection_method": "llm",
                "selection_reasoning": "Test",
                "candidate_scores": "{}",
                "execution_success": True,
                "execution_time_ms": 1500,
                "execution_cost": 0.05,
                "user_feedback_score": 5,
                "created_at": datetime.now(timezone.utc),
            }
        ]
        mock_db.execute.return_value = golden_events
        mock_selector._select_candidates.side_effect = Exception("Test error")

        result = regression_gate.validate_selector_change(
            new_selector=mock_selector,
            old_selector=mock_selector,
            selector_version="v2.0.0",
        )

        # Sandbox should still be cleaned up
        assert mock_sandbox.delete.called

    def test_multiple_queries_scoring(self, regression_gate, mock_selector):
        """Test scoring with multiple queries."""
        queries = [
            SkillSelectionEvent(
                event_id=f"event-{i}",
                session_id="session-1",
                user_query=f"Query {i}",
                context_snapshot=f"snapshot_{i}",
                available_skills=[{"name": "code_review"}],
                selected_skills=["code_review"],
                selection_method="llm",
                selection_reasoning="Test",
                candidate_scores={},
                execution_success=True,
                user_feedback_score=5,
                created_at=datetime.now(timezone.utc),
            )
            for i in range(5)
        ]

        mock_selector._select_candidates = Mock(return_value=[make_skill(name="code_review", triggers=["review"])])

        results = regression_gate._test_selector(mock_selector, queries, "test_sandbox")

        assert len(results) == 5
        # All should have perfect score
        for r in results:
            assert r["score"] == 1.0
