"""Tests for auditable skill selector with Git for Data integration."""

import json
from datetime import datetime, timezone
from unittest.mock import Mock, patch

import pytest

from core.skills.auditable_selector import AuditableSkillSelector, SkillSelectionEvent
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
    llm.chat_with_tools = Mock(return_value={"tool_calls": []})
    return llm


@pytest.fixture
def mock_sandbox():
    """Mock sandbox."""
    sandbox = Mock()
    sandbox.create = Mock()
    sandbox.delete = Mock()
    return sandbox


@pytest.fixture
def mock_modern_selector():
    """Mock modern selector."""
    selector = Mock()
    selector.rule_selector.select_skills = Mock(return_value=[])
    return selector


@pytest.fixture
def auditable_selector(mock_db, mock_llm, mock_sandbox, mock_modern_selector):
    """Auditable skill selector instance."""
    with patch("core.skills.auditable_selector.Sandbox", return_value=mock_sandbox):
        with patch("core.skills.auditable_selector.ModernSkillSelector", return_value=mock_modern_selector):
            selector = AuditableSkillSelector(mock_db, mock_llm)
            selector.sandbox = mock_sandbox
            selector.modern_selector = mock_modern_selector
            return selector


class TestSkillSelectionEvent:
    """Test SkillSelectionEvent dataclass."""

    def test_event_creation(self):
        """Test creating a skill selection event."""
        event = SkillSelectionEvent(
            event_id="test-123",
            session_id="session-456",
            user_query="Review PR #123",
            context_snapshot="snapshot_abc",
            available_skills=[{"name": "code_review", "version": "1.0.0"}],
            selected_skills=["code_review"],
            selection_method="llm",
            selection_reasoning="Selected based on query",
            candidate_scores={"code_review": 0.9},
        )

        assert event.event_id == "test-123"
        assert event.session_id == "session-456"
        assert event.user_query == "Review PR #123"
        assert event.context_snapshot == "snapshot_abc"
        assert event.selected_skills == ["code_review"]
        assert event.selection_method == "llm"
        assert event.candidate_scores == {"code_review": 0.9}
        assert event.execution_success is None
        assert event.user_feedback_score is None

    def test_event_with_execution_result(self):
        """Test event with execution result."""
        event = SkillSelectionEvent(
            event_id="test-123",
            session_id="session-456",
            user_query="Review PR #123",
            context_snapshot="snapshot_abc",
            available_skills=[],
            selected_skills=["code_review"],
            selection_method="llm",
            selection_reasoning="Test",
            candidate_scores={},
            execution_success=True,
            execution_time_ms=1500,
            execution_cost=0.05,
            execution_result={"status": "completed"},
        )

        assert event.execution_success is True
        assert event.execution_time_ms == 1500
        assert event.execution_cost == 0.05
        assert event.execution_result == {"status": "completed"}


class TestAuditableSkillSelector:
    """Test AuditableSkillSelector class."""

    def test_init(self, auditable_selector, mock_db, mock_llm):
        """Test selector initialization."""
        assert auditable_selector.db is mock_db
        assert auditable_selector.llm is mock_llm
        assert auditable_selector.account == "sys"

    def test_ensure_table(self, auditable_selector, mock_db):
        """Test table creation."""
        mock_db.execute.reset_mock()
        auditable_selector._ensure_table()
        assert mock_db.execute.called
        call_args = mock_db.execute.call_args[0][0]
        assert "skill_selection_events" in call_args

    def test_select_with_validation_no_candidates(self, auditable_selector, mock_db, mock_modern_selector):
        """Test selection when no candidates found."""
        mock_modern_selector.rule_selector.select_skills = Mock(return_value=[])

        event = auditable_selector.select_with_validation(
            query="unrelated query",
            session_id="test-session",
            validate_in_sandbox=True,
        )

        assert event.selected_skills == []
        assert event.selection_method == "none"
        assert event.session_id == "test-session"

    def test_select_with_validation_single_candidate(self, auditable_selector, mock_db, mock_modern_selector):
        """Test selection with single candidate (no validation needed)."""
        skills = [make_skill(name="code_review", triggers=["review"])]
        mock_modern_selector.rule_selector.select_skills = Mock(return_value=skills)

        event = auditable_selector.select_with_validation(
            query="Review PR #123",
            session_id="test-session",
            validate_in_sandbox=True,
        )

        assert event.selected_skills == ["code_review"]
        assert event.selection_method == "llm"

    def test_select_with_validation_multiple_candidates(self, auditable_selector, mock_db, mock_modern_selector, mock_sandbox):
        """Test selection with multiple candidates triggers validation."""
        skills = [
            make_skill(name="code_review", triggers=["review"]),
            make_skill(name="summarize_pr", priority=6, cost_estimate="low"),
        ]
        mock_modern_selector.rule_selector.select_skills = Mock(return_value=skills)

        event = auditable_selector.select_with_validation(
            query="Review PR #123",
            session_id="test-session",
            validate_in_sandbox=True,
        )

        # Should validate in sandbox
        assert mock_sandbox.create.called
        assert event.selection_method == "validated"

    def test_create_selection_snapshot(self, auditable_selector, mock_db):
        """Test snapshot creation."""
        snapshot_id = auditable_selector._create_selection_snapshot("session-123", "event-456")
        assert "skill_select_session-123_event" in snapshot_id or "snapshot_" in snapshot_id

    def test_get_available_skills(self, auditable_selector, mock_db):
        """Test getting available skills."""
        mock_db.execute.return_value = [
            {
                "skill_name": "code_review",
                "version": "1.0.0",
                "description": "Review code",
                "category": "github",
                "subcategory": "pr",
                "triggers": '["review"]',
                "dependencies": '[]',
                "priority": 8,
                "cost_estimate": "medium",
            }
        ]

        skills = auditable_selector._get_available_skills()

        assert len(skills) == 1
        assert skills[0].name == "code_review"
        assert skills[0].priority == 8

    def test_dry_run_skill(self, auditable_selector):
        """Test dry run skill simulation."""
        skill = make_skill(name="code_review", triggers=["review"])

        result = auditable_selector._dry_run_skill("sandbox_name", skill, "test query", "snapshot")

        assert "success" in result
        assert "score" in result
        assert "time_ms" in result
        assert "cost" in result

    def test_dry_run_skill_low_priority(self, auditable_selector):
        """Test dry run with low priority skill."""
        skill = make_skill(name="test_skill", priority=3, cost_estimate="low")

        result = auditable_selector._dry_run_skill("sandbox", skill, "test", "snapshot")

        # Low priority should have lower success rate
        assert result["success"] is False
        assert result["score"] == 0.0

    def test_validate_in_sandbox(self, auditable_selector, mock_sandbox):
        """Test sandbox validation."""
        skills = [make_skill(name="code_review", triggers=["review"])]

        result = auditable_selector._validate_in_sandbox(
            skills, "Review PR #123", "snapshot_abc", "event-123"
        )

        assert "selected" in result
        assert "scores" in result
        assert "reasoning" in result
        assert mock_sandbox.create.called
        assert mock_sandbox.delete.called

    def test_save_event(self, auditable_selector, mock_db):
        """Test saving event to database."""
        now = datetime.now(timezone.utc)
        event = SkillSelectionEvent(
            event_id="test-123",
            session_id="session-456",
            user_query="Review PR #123",
            context_snapshot="snapshot_abc",
            available_skills=[{"name": "code_review"}],
            selected_skills=["code_review"],
            selection_method="llm",
            selection_reasoning="Test",
            candidate_scores={"code_review": 0.9},
            created_at=now,
        )

        auditable_selector._save_event(event)

        assert mock_db.execute.called
        call_args = mock_db.execute.call_args[0]
        assert "INSERT INTO" in call_args[0]
        assert call_args[1][0] == "test-123"

    def test_update_execution_result(self, auditable_selector, mock_db):
        """Test updating execution result."""
        auditable_selector.update_execution_result(
            event_id="test-123",
            success=True,
            time_ms=1500,
            cost=0.05,
            result={"status": "completed"},
        )

        assert mock_db.execute.called
        call_args = mock_db.execute.call_args[0]
        assert "UPDATE" in call_args[0]
        assert call_args[1][0] is True

    def test_update_user_feedback_valid(self, auditable_selector, mock_db):
        """Test updating user feedback with valid score."""
        mock_db.execute.reset_mock()
        auditable_selector.update_user_feedback(event_id="test-123", score=5)

        # Should update feedback and correctness
        assert mock_db.execute.call_count == 2

    def test_update_user_feedback_invalid(self, auditable_selector):
        """Test updating user feedback with invalid score."""
        with pytest.raises(ValueError, match="Score must be between 1 and 5"):
            auditable_selector.update_user_feedback(event_id="test-123", score=6)

        with pytest.raises(ValueError, match="Score must be between 1 and 5"):
            auditable_selector.update_user_feedback(event_id="test-123", score=0)

    def test_get_selection_history(self, auditable_selector, mock_db):
        """Test getting selection history."""
        mock_db.execute.return_value = [
            {
                "event_id": "test-123",
                "session_id": "session-456",
                "user_query": "Review PR #123",
                "context_snapshot": "snapshot_abc",
                "available_skills": '["code_review"]',
                "selected_skills": '["code_review"]',
                "selection_method": "llm",
                "selection_reasoning": "Test",
                "candidate_scores": "{}",
                "execution_result": None,
                "execution_success": True,
                "execution_time_ms": 1500,
                "execution_cost": 0.05,
                "user_feedback_score": 5,
                "selection_correctness": True,
                "correction_suggestion": None,
                "created_at": datetime.now(timezone.utc),
            }
        ]

        events = auditable_selector.get_selection_history(session_id="session-456")

        assert len(events) == 1
        assert events[0].event_id == "test-123"
        assert events[0].selected_skills == ["code_review"]

    def test_get_selection_history_with_limit(self, auditable_selector, mock_db):
        """Test getting selection history with limit."""
        auditable_selector.get_selection_history(limit=50)

        call_args = mock_db.execute.call_args[0]
        assert "LIMIT %s" in call_args[0]
        assert call_args[1][-1] == 50


class TestAuditableSelectorEdgeCases:
    """Test edge cases for auditable selector."""

    def test_sandbox_creation_failure(self, auditable_selector, mock_db, mock_modern_selector, mock_sandbox):
        """Test handling sandbox creation failure."""
        skills = [make_skill(name="code_review", triggers=["review"])]
        mock_modern_selector.rule_selector.select_skills = Mock(return_value=skills)
        mock_sandbox.create.side_effect = Exception("Sandbox creation failed")

        # Should not raise, should fallback
        event = auditable_selector.select_with_validation(
            query="Review PR #123",
            session_id="test-session",
            validate_in_sandbox=True,
        )

        assert event.selection_method == "llm"

    def test_empty_available_skills(self, auditable_selector, mock_db):
        """Test handling when no skills are available."""
        mock_db.execute.return_value = []

        skills = auditable_selector._get_available_skills()

        assert skills == []

    def test_event_with_all_fields(self, auditable_selector, mock_db):
        """Test event with all optional fields populated."""
        now = datetime.now(timezone.utc)
        event = SkillSelectionEvent(
            event_id="test-123",
            session_id="session-456",
            user_query="Review PR #123",
            context_snapshot="snapshot_abc",
            available_skills=[{"name": "code_review"}],
            selected_skills=["code_review"],
            selection_method="validated",
            selection_reasoning="Validated in sandbox",
            candidate_scores={"code_review": 0.95, "summarize_pr": 0.5},
            execution_success=True,
            execution_time_ms=2000,
            execution_cost=0.08,
            execution_result={"issues_found": 5},
            user_feedback_score=5,
            selection_correctness=True,
            correction_suggestion=["security_review"],
            created_at=now,
        )

        assert event.execution_result == {"issues_found": 5}
        assert event.correction_suggestion == ["security_review"]
