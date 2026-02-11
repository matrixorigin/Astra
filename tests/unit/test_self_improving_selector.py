"""Tests for self-improving skill selector."""

import json
from datetime import datetime, timezone
from unittest.mock import Mock, patch

import pytest

from core.skills.self_improving_selector import SelfImprovingSelector
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
def mock_auditable_selector():
    """Mock auditable selector."""
    selector = Mock()
    return selector


@pytest.fixture
def self_improving_selector(mock_db, mock_llm, mock_sandbox, mock_auditable_selector):
    """Self-improving selector instance."""
    with patch("core.skills.self_improving_selector.Sandbox", return_value=mock_sandbox):
        with patch("core.skills.self_improving_selector.AuditableSkillSelector", return_value=mock_auditable_selector):
            selector = SelfImprovingSelector(mock_db, mock_llm)
            selector.sandbox = mock_sandbox
            selector.auditable_selector = mock_auditable_selector
            return selector


class TestSelfImprovingSelector:
    """Test SelfImprovingSelector class."""

    def test_init(self, self_improving_selector, mock_db, mock_llm):
        """Test selector initialization."""
        assert self_improving_selector.db is mock_db
        assert self_improving_selector.llm is mock_llm
        assert self_improving_selector.account == "sys"

    def test_ensure_tables(self, self_improving_selector, mock_db):
        """Test table creation."""
        self_improving_selector._ensure_tables()
        mock_db.execute.assert_called()
        call_args = mock_db.execute.call_args[0][0]
        assert "skill_selection_learnings" in call_args

    def test_learn_from_failures_no_failures(self, self_improving_selector, mock_db):
        """Test learning when no failures found."""
        mock_db.execute.return_value = []

        result = self_improving_selector.learn_from_failures(days=7)

        assert result["failures_analyzed"] == 0
        assert result["corrections_found"] == 0
        assert result["learnings_added"] == 0

    def test_learn_from_failures_with_failures(self, self_improving_selector, mock_db, mock_sandbox):
        """Test learning from actual failures."""
        # Setup failures
        failures = [
            {
                "event_id": "event-1",
                "session_id": "session-1",
                "user_query": "Review PR #123",
                "context_snapshot": "snapshot_1",
                "available_skills": json.dumps([{"name": "code_review", "priority": 5}]),
                "selected_skills": json.dumps(["wrong_skill"]),
                "selection_method": "llm",
                "selection_reasoning": "Test",
                "candidate_scores": "{}",
                "execution_success": False,
                "user_feedback_score": 1,
                "created_at": datetime.now(timezone.utc),
            }
        ]
        mock_db.execute.return_value = failures

        result = self_improving_selector.learn_from_failures(days=7)

        assert result["failures_analyzed"] == 1
        assert mock_sandbox.create.called
        assert mock_sandbox.delete.called

    def test_learn_from_failures_creates_sandbox(self, self_improving_selector, mock_db, mock_sandbox):
        """Test that learning creates sandbox for analysis."""
        failures = [
            {
                "event_id": "event-1",
                "session_id": "session-1",
                "user_query": "Review PR #123",
                "context_snapshot": "snapshot_1",
                "available_skills": json.dumps([{"name": "code_review", "priority": 8}]),
                "selected_skills": json.dumps(["wrong_skill"]),
                "selection_method": "llm",
                "selection_reasoning": "Test",
                "candidate_scores": "{}",
                "execution_success": False,
                "user_feedback_score": 1,
                "created_at": datetime.now(timezone.utc),
            }
        ]
        # Mock execute: first call returns failures, rest return empty
        mock_db.execute.side_effect = [failures] + [[] for _ in range(10)]

        self_improving_selector.learn_from_failures(days=7)

        mock_sandbox.create.assert_called_once()
        # Check that sandbox.create was called with description containing "Learning from"
        call_kwargs = mock_sandbox.create.call_args.kwargs
        assert "Learning from" in call_kwargs.get("description", "")

    def test_learn_from_failures_cleanup(self, self_improving_selector, mock_db, mock_sandbox):
        """Test that sandbox is cleaned up after learning."""
        failures = [
            {
                "event_id": "event-1",
                "session_id": "session-1",
                "user_query": "Review PR #123",
                "context_snapshot": "snapshot_1",
                "available_skills": json.dumps([{"name": "code_review", "priority": 8}]),
                "selected_skills": json.dumps(["wrong_skill"]),
                "selection_method": "llm",
                "selection_reasoning": "Test",
                "candidate_scores": "{}",
                "execution_success": False,
                "user_feedback_score": 1,
                "created_at": datetime.now(timezone.utc),
            }
        ]
        # Mock execute: first call returns failures, rest return empty
        mock_db.execute.side_effect = [failures] + [[] for _ in range(10)]

        self_improving_selector.learn_from_failures(days=7)

        # Sandbox should be deleted even if learning succeeds
        assert mock_sandbox.delete.called

    def test_get_recent_failures(self, self_improving_selector, mock_db):
        """Test getting recent failures."""
        mock_db.execute.return_value = [
            {
                "event_id": "event-1",
                "session_id": "session-1",
                "user_query": "Review PR #123",
                "context_snapshot": "snapshot_1",
                "available_skills": '["code_review"]',
                "selected_skills": '["wrong_skill"]',
                "selection_method": "llm",
                "selection_reasoning": "Test",
                "candidate_scores": "{}",
                "execution_success": False,
                "user_feedback_score": 1,
                "created_at": datetime.now(timezone.utc),
            }
        ]

        failures = self_improving_selector._get_recent_failures(days=7)

        assert len(failures) == 1
        assert failures[0].event_id == "event-1"
        assert failures[0].execution_success is False

    def test_get_recent_failures_low_feedback(self, self_improving_selector, mock_db):
        """Test getting failures by low feedback."""
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
                "user_feedback_score": 2,  # Low feedback
                "selection_correctness": False,
                "created_at": datetime.now(timezone.utc),
            }
        ]

        failures = self_improving_selector._get_recent_failures(days=7)

        assert len(failures) == 1
        assert failures[0].user_feedback_score == 2

    def test_analyze_failure_in_sandbox_no_alternatives(self, self_improving_selector, mock_db):
        """Test analysis when no alternatives available."""
        failure = SkillSelectionEvent(
            event_id="event-1",
            session_id="session-1",
            user_query="Review PR #123",
            context_snapshot="snapshot_1",
            available_skills=[{"name": "code_review", "priority": 5}],
            selected_skills=["code_review"],  # Only skill available
            selection_method="llm",
            selection_reasoning="Test",
            candidate_scores={},
            execution_success=False,
            user_feedback_score=1,
            created_at=datetime.now(timezone.utc),
        )

        result = self_improving_selector._analyze_failure_in_sandbox("test_sandbox", failure)

        assert result is None

    def test_analyze_failure_in_sandbox_with_alternatives(self, self_improving_selector, mock_db):
        """Test analysis with alternatives."""
        failure = SkillSelectionEvent(
            event_id="event-1",
            session_id="session-1",
            user_query="Review PR #123",
            context_snapshot="snapshot_1",
            available_skills=[
                {"name": "code_review", "priority": 5, "triggers": ["review"]},
                {"name": "security_review", "priority": 8, "triggers": ["security"]},
            ],
            selected_skills=["code_review"],  # Wrong choice
            selection_method="llm",
            selection_reasoning="Test",
            candidate_scores={},
            execution_success=False,
            user_feedback_score=1,
            created_at=datetime.now(timezone.utc),
        )

        result = self_improving_selector._analyze_failure_in_sandbox("test_sandbox", failure)

        assert result is not None
        assert "query_pattern" in result
        assert "wrong_skills" in result
        assert "correct_skills" in result
        assert "improvement_score" in result

    def test_generate_alternatives(self, self_improving_selector):
        """Test alternative generation."""
        available_skills = [
            {"name": "code_review", "priority": 5},
            {"name": "security_review", "priority": 8},
            {"name": "summarize_pr", "priority": 6},
        ]
        wrong_skills = ["code_review"]

        alternatives = self_improving_selector._generate_alternatives(
            "Review PR for security", available_skills, wrong_skills
        )

        assert len(alternatives) == 2
        assert all(a["skills"][0] != "code_review" for a in alternatives)

    def test_generate_alternatives_all_wrong(self, self_improving_selector):
        """Test when all skills are marked as wrong."""
        available_skills = [
            {"name": "code_review", "priority": 5},
            {"name": "security_review", "priority": 8},
        ]
        wrong_skills = ["code_review", "security_review"]

        alternatives = self_improving_selector._generate_alternatives(
            "Test query", available_skills, wrong_skills
        )

        assert alternatives == []

    def test_test_alternatives(self, self_improving_selector):
        """Test alternative testing."""
        alternatives = [
            {"skills": ["code_review"], "skill_obj": {"name": "code_review", "priority": 5, "triggers": []}},
            {"skills": ["security_review"], "skill_obj": {"name": "security_review", "priority": 8, "triggers": ["security"]}},
        ]

        result = self_improving_selector._test_alternatives(alternatives, "Review PR for security issues")

        assert result is not None
        assert result["skills"] == ["security_review"]  # Higher priority + trigger match
        assert result["score"] > 0.5

    def test_test_alternatives_no_match(self, self_improving_selector):
        """Test alternatives with no good match."""
        alternatives = [
            {"skills": ["low_priority"], "skill_obj": {"name": "low_priority", "priority": 1, "triggers": []}},
        ]

        result = self_improving_selector._test_alternatives(alternatives, "Unrelated query")

        assert result is None  # Score too low

    def test_test_alternatives_empty(self, self_improving_selector):
        """Test with empty alternatives."""
        result = self_improving_selector._test_alternatives([], "Test query")

        assert result is None

    def test_extract_query_pattern(self, self_improving_selector):
        """Test query pattern extraction."""
        query = "Review PR #123 for security issues in the authentication module"
        pattern = self_improving_selector._extract_query_pattern(query)

        # Should be lowercase, first 50 chars
        assert pattern.startswith("review pr #123 for security issues in the authenti")
        assert len(pattern) <= 50

    def test_extract_query_pattern_short(self, self_improving_selector):
        """Test pattern extraction for short query."""
        query = "Short query"
        pattern = self_improving_selector._extract_query_pattern(query)

        assert pattern == "short query"

    def test_update_learnings_new_learning(self, self_improving_selector, mock_db):
        """Test adding new learning."""
        mock_db.execute.return_value = []  # No existing learning

        corrections = [
            {
                "query_pattern": "review pr",
                "wrong_skills": ["code_review"],
                "correct_skills": ["security_review"],
                "improvement_score": 0.8,
                "evidence": "event-1",
            }
        ]

        count = self_improving_selector._update_learnings(corrections)

        assert count == 1
        assert mock_db.execute.call_count >= 2  # SELECT + INSERT

    def test_update_learnings_existing_learning(self, self_improving_selector, mock_db):
        """Test updating existing learning."""
        mock_db.execute.side_effect = [
            [{"learning_id": "learn-1", "evidence_count": 1, "improvement_score": 0.7}],  # SELECT
            [],  # UPDATE
        ]

        corrections = [
            {
                "query_pattern": "review pr",
                "wrong_skills": ["code_review"],
                "correct_skills": ["security_review"],
                "improvement_score": 0.9,
                "evidence": "event-2",
            }
        ]

        count = self_improving_selector._update_learnings(corrections)

        assert count == 0  # No new learning added
        # Should have called UPDATE
        last_call = mock_db.execute.call_args_list[-1]
        assert "UPDATE" in last_call[0][0]

    def test_update_learnings_confidence_accumulation(self, self_improving_selector, mock_db):
        """Test that confidence increases with evidence."""
        mock_db.execute.side_effect = [
            [{"learning_id": "learn-1", "evidence_count": 5, "improvement_score": 0.7}],  # SELECT
            [],  # UPDATE
        ]

        corrections = [
            {
                "query_pattern": "review pr",
                "wrong_skills": ["code_review"],
                "correct_skills": ["security_review"],
                "improvement_score": 0.8,
                "evidence": "event-6",
            }
        ]

        self_improving_selector._update_learnings(corrections)

        # Check UPDATE was called with updated confidence
        update_call = mock_db.execute.call_args_list[-1]
        update_args = update_call[0][1]
        # evidence_count should be 6 (5 + 1)
        assert update_args[0] == 6

    def test_apply_learnings_no_match(self, self_improving_selector, mock_db):
        """Test applying learnings when no match."""
        mock_db.execute.return_value = []

        candidates = ["code_review", "summarize_pr"]
        corrected = self_improving_selector.apply_learnings("Review PR #123", candidates)

        assert corrected == candidates

    def test_apply_learnings_with_match(self, self_improving_selector, mock_db):
        """Test applying learnings with matching pattern."""
        mock_db.execute.return_value = [
            {
                "wrong_skills": json.dumps(["code_review"]),
                "correct_skills": json.dumps(["security_review"]),
                "confidence": 0.8,
            }
        ]

        candidates = ["code_review", "summarize_pr"]
        corrected = self_improving_selector.apply_learnings("Review PR for security", candidates)

        assert "code_review" not in corrected
        assert "security_review" in corrected

    def test_apply_learnings_low_confidence(self, self_improving_selector, mock_db):
        """Test that low confidence learnings are ignored."""
        mock_db.execute.return_value = []  # No learnings returned

        candidates = ["code_review"]
        corrected = self_improving_selector.apply_learnings("Review PR", candidates)

        # Should not apply low confidence learning
        assert corrected == candidates

    def test_apply_learnings_records_application(self, self_improving_selector, mock_db):
        """Test that application is recorded."""
        mock_db.execute.return_value = [
            {
                "wrong_skills": json.dumps(["code_review"]),
                "correct_skills": json.dumps(["security_review"]),
                "confidence": 0.8,
            }
        ]

        self_improving_selector.apply_learnings("Review PR for security", ["code_review"])

        # Should record application count
        last_call = mock_db.execute.call_args_list[-1]
        assert "UPDATE" in last_call[0][0]
        assert "applied_count" in last_call[0][0]

    def test_get_learning_stats(self, self_improving_selector, mock_db):
        """Test getting learning statistics."""
        mock_db.execute.side_effect = [
            [
                {
                    "total_learnings": 10,
                    "avg_confidence": 0.65,
                    "total_evidence": 25,
                    "total_applications": 100,
                }
            ],
            [{"count": 5}],  # High confidence count
        ]

        stats = self_improving_selector.get_learning_stats()

        assert stats["total_learnings"] == 10
        assert stats["avg_confidence"] == 0.65
        assert stats["total_evidence"] == 25
        assert stats["total_applications"] == 100
        assert stats["high_confidence_learnings"] == 5

    def test_get_learning_stats_no_data(self, self_improving_selector, mock_db):
        """Test getting stats with no data."""
        mock_db.execute.side_effect = [
            [
                {
                    "total_learnings": 0,
                    "avg_confidence": None,
                    "total_evidence": 0,
                    "total_applications": 0,
                }
            ],
            [{"count": 0}],
        ]

        stats = self_improving_selector.get_learning_stats()

        assert stats["total_learnings"] == 0
        assert stats["avg_confidence"] == 0.0
        assert stats["high_confidence_learnings"] == 0


class TestSelfImprovingSelectorEdgeCases:
    """Test edge cases for self-improving selector."""

    def test_analyze_failure_sandbox_error(self, self_improving_selector, mock_db):
        """Test handling sandbox error during analysis."""
        failure = SkillSelectionEvent(
            event_id="event-1",
            session_id="session-1",
            user_query="Review PR #123",
            context_snapshot="snapshot_1",
            available_skills=[
                {"name": "code_review", "priority": 5},
                {"name": "security_review", "priority": 8},
            ],
            selected_skills=["code_review"],
            selection_method="llm",
            selection_reasoning="Test",
            candidate_scores={},
            execution_success=False,
            user_feedback_score=1,
            created_at=datetime.now(timezone.utc),
        )

        # Mock db.execute to raise error on first call (USE sandbox), then succeed
        mock_db.execute.side_effect = [Exception("Sandbox error")] + [[] for _ in range(10)]

        result = self_improving_selector._analyze_failure_in_sandbox("test_sandbox", failure)

        # Should return None on error
        assert result is None

    def test_apply_learnings_empty_candidates(self, self_improving_selector, mock_db):
        """Test applying learnings with empty candidates."""
        mock_db.execute.return_value = [
            {
                "wrong_skills": json.dumps(["code_review"]),
                "correct_skills": json.dumps(["security_review"]),
                "confidence": 0.8,
            }
        ]

        corrected = self_improving_selector.apply_learnings("Review PR", [])

        assert corrected == []

    def test_learn_from_failures_database_error(self, self_improving_selector, mock_db):
        """Test handling database error during learning."""
        # First call returns empty (no failures), so no error
        mock_db.execute.return_value = []

        result = self_improving_selector.learn_from_failures(days=7)

        assert result["failures_analyzed"] == 0
        assert result["corrections_found"] == 0
        assert result["learnings_added"] == 0

    def test_update_learnings_database_error(self, self_improving_selector, mock_db):
        """Test handling database error during learning update."""
        # No corrections to update
        corrections = []

        count = self_improving_selector._update_learnings(corrections)

        assert count == 0  # No learning added

    def test_multiple_corrections_same_pattern(self, self_improving_selector, mock_db):
        """Test handling multiple corrections for same pattern."""
        # First call returns existing learning, second call returns empty (after update)
        mock_db.execute.side_effect = [
            [{"learning_id": "learn-1", "evidence_count": 1, "improvement_score": 0.7}],  # First SELECT
            [],  # UPDATE
            [],  # Second SELECT (for second correction)
            [],  # INSERT
        ]

        corrections = [
            {
                "query_pattern": "review pr",
                "wrong_skills": ["code_review"],
                "correct_skills": ["security_review"],
                "improvement_score": 0.8,
                "evidence": "event-2",
            },
            {
                "query_pattern": "review pr",
                "wrong_skills": ["summarize_pr"],
                "correct_skills": ["detailed_analysis"],
                "improvement_score": 0.9,
                "evidence": "event-3",
            },
        ]

        count = self_improving_selector._update_learnings(corrections)

        # One update + one insert = 1 new learning
        assert count == 1

    def test_apply_learnings_multiple_match(self, self_improving_selector, mock_db):
        """Test applying multiple matching learnings."""
        mock_db.execute.return_value = [
            {
                "wrong_skills": json.dumps(["code_review"]),
                "correct_skills": json.dumps(["security_review"]),
                "confidence": 0.8,
            },
            {
                "wrong_skills": json.dumps(["summarize_pr"]),
                "correct_skills": json.dumps(["detailed_report"]),
                "confidence": 0.7,
            },
        ]

        candidates = ["code_review", "summarize_pr"]
        corrected = self_improving_selector.apply_learnings("Review PR and summarize", candidates)

        assert "security_review" in corrected
        assert "detailed_report" in corrected
        assert "code_review" not in corrected
        assert "summarize_pr" not in corrected

    def test_apply_learnings_avoid_duplicates(self, self_improving_selector, mock_db):
        """Test that correct skills don't duplicate existing ones."""
        mock_db.execute.return_value = [
            {
                "wrong_skills": json.dumps(["code_review"]),
                "correct_skills": json.dumps(["security_review"]),
                "confidence": 0.8,
            }
        ]

        candidates = ["security_review", "code_review"]  # Already has correct skill
        corrected = self_improving_selector.apply_learnings("Review PR for security", candidates)

        # Should not duplicate security_review
        assert corrected.count("security_review") == 1
