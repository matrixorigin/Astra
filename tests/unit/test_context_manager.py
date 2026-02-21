"""Unit tests for ContextManager with caching functionality."""

import os
from unittest.mock import MagicMock, patch

import pytest

from core.context.manager import Context, ContextFragment, ContextManager, TaskType


@pytest.fixture
def mock_db():
    """Create a mock database."""
    db = MagicMock()
    return db


@pytest.fixture
def context_manager(mock_db):
    """Create ContextManager with mocked dependencies."""
    with patch("core.context.embeddings.EmbeddingService"):
        with patch("core.context.prompts.PromptManager"):
            with patch("core.context.scorer.RelevanceScorer"):
                cm = ContextManager(db=mock_db, embedding_provider="mock")
                yield cm


class TestContextManagerInit:
    """Tests for ContextManager initialization."""

    def test_initialization_with_defaults(self, mock_db):
        """Test ContextManager initializes with default values."""
        with patch("core.context.embeddings.EmbeddingService"):
            with patch("core.context.prompts.PromptManager"):
                with patch("core.context.scorer.RelevanceScorer"):
                    cm = ContextManager(db=mock_db)
                    assert cm.db == mock_db


class TestTaskType:
    """Tests for TaskType enum."""

    def test_task_type_values(self):
        """Test TaskType has expected values."""
        assert TaskType.CODE_REVIEW.value == "code_review"
        assert TaskType.PLANNING.value == "planning"
        assert TaskType.DEBUGGING.value == "debugging"
        assert TaskType.GENERAL.value == "general"

    def test_task_type_from_string(self):
        """Test creating TaskType from string."""
        assert TaskType("code_review") == TaskType.CODE_REVIEW
        assert TaskType("planning") == TaskType.PLANNING


class TestClassifyTask:
    """Tests for auto task classification."""

    def test_code_review_keywords(self):
        assert ContextManager.classify_task("Please review this PR") == TaskType.CODE_REVIEW
        assert ContextManager.classify_task("code review for auth module") == TaskType.CODE_REVIEW
        assert ContextManager.classify_task("refactor the parser") == TaskType.CODE_REVIEW

    def test_debugging_keywords(self):
        assert ContextManager.classify_task("debug this error") == TaskType.DEBUGGING
        assert ContextManager.classify_task("fix the crash in login") == TaskType.DEBUGGING
        assert ContextManager.classify_task("there's a traceback here") == TaskType.DEBUGGING

    def test_planning_keywords(self):
        assert ContextManager.classify_task("plan the migration") == TaskType.PLANNING
        assert ContextManager.classify_task("design a new API") == TaskType.PLANNING
        assert ContextManager.classify_task("create a roadmap") == TaskType.PLANNING

    def test_general_fallback(self):
        assert ContextManager.classify_task("hello world") == TaskType.GENERAL
        assert ContextManager.classify_task("what is this?") == TaskType.GENERAL

    def test_case_insensitive(self):
        assert ContextManager.classify_task("DEBUG this") == TaskType.DEBUGGING
        assert ContextManager.classify_task("REVIEW my code") == TaskType.CODE_REVIEW

    def test_first_match_wins(self):
        """When multiple keywords match, first TaskType in iteration order wins."""
        result = ContextManager.classify_task("review and debug this")
        assert result in (TaskType.CODE_REVIEW, TaskType.DEBUGGING)


class TestContextFragment:
    """Tests for ContextFragment dataclass."""

    def test_context_fragment_creation(self):
        """Test ContextFragment dataclass creation."""
        fragment = ContextFragment(
            content="test content",
            tokens=10,
            source="event",
            relevance_score=0.8,
            metadata={"key": "value"},
        )

        assert fragment.content == "test content"
        assert fragment.tokens == 10
        assert fragment.source == "event"
        assert fragment.relevance_score == 0.8
        assert fragment.metadata == {"key": "value"}



class TestAllocateBudget:
    """Test token budget allocation."""

    def test_code_review_budget(self, context_manager):
        """Test CODE_REVIEW budget allocation (design: code 50%, history 20%, docs 20%, logs 10%)."""
        budget = context_manager._allocate_budget(10000, TaskType.CODE_REVIEW)

        assert budget["system"] == {"allocated": 500, "used": 0}
        assert budget["skills"] == {"allocated": 1000, "used": 0}
        assert budget["reserve"] == {"allocated": 500, "used": 0}

        available = 8000
        assert budget["code"]["allocated"] == int(available * 0.50)
        assert budget["history"]["allocated"] == int(available * 0.20)
        assert budget["docs"]["allocated"] == int(available * 0.20)
        assert budget["logs"]["allocated"] == int(available * 0.10)

    def test_planning_budget(self, context_manager):
        """Test PLANNING budget allocation (design: history 50%, code 20%, docs 20%, logs 10%)."""
        budget = context_manager._allocate_budget(10000, TaskType.PLANNING)
        available = 8000
        assert budget["history"]["allocated"] == int(available * 0.50)
        assert budget["code"]["allocated"] == int(available * 0.20)
        assert budget["docs"]["allocated"] == int(available * 0.20)

    def test_debugging_budget(self, context_manager):
        """Test DEBUGGING budget allocation (design: logs 40%, code 30%, history 20%, docs 10%)."""
        budget = context_manager._allocate_budget(10000, TaskType.DEBUGGING)
        available = 8000
        assert budget["logs"]["allocated"] == int(available * 0.40)
        assert budget["code"]["allocated"] == int(available * 0.30)
        assert budget["history"]["allocated"] == int(available * 0.20)

    def test_general_budget(self, context_manager):
        """Test GENERAL budget allocation (history 40%, code 30%, docs 20%, logs 10%)."""
        budget = context_manager._allocate_budget(10000, TaskType.GENERAL)
        available = 8000
        assert budget["history"]["allocated"] == int(available * 0.40)
        assert budget["code"]["allocated"] == int(available * 0.30)
        assert budget["docs"]["allocated"] == int(available * 0.20)

    def test_budget_with_small_total(self, context_manager):
        """Test budget allocation with small total tokens."""
        budget = context_manager._allocate_budget(2000, TaskType.GENERAL)
        assert budget["system"]["allocated"] == 500
        assert budget["skills"]["allocated"] == 1000
        assert budget["reserve"]["allocated"] == 500
        assert budget["history"]["allocated"] >= 0
        assert budget["code"]["allocated"] >= 0
        assert budget["docs"]["allocated"] >= 0

    def test_budget_returns_used_zero(self, context_manager):
        """All sections start with used=0."""
        budget = context_manager._allocate_budget(10000, TaskType.GENERAL)
        for section in budget.values():
            assert section["used"] == 0


class TestGetSkillDefinitions:
    """Tests for _get_skill_definitions loading from DB."""

    def test_loads_active_skills(self, context_manager, mock_db):
        """Should query active skills from DB and return structured dicts."""
        skill = MagicMock()
        skill.skill_name = "code_review"
        skill.description = "Review code"
        skill.version = "1.0.0"
        skill.skill_definition = {"repo_types": ["code"]}
        skill.triggers = ["review", "pr"]

        mock_db.query.return_value.filter.return_value.all.return_value = [skill]

        result = context_manager._get_skill_definitions(4000)

        assert len(result) == 1
        assert result[0]["skill_name"] == "code_review"
        assert result[0]["definition"] == {"repo_types": ["code"]}
        assert result[0]["triggers"] == ["review", "pr"]

    def test_returns_empty_on_db_error(self, context_manager, mock_db):
        """Should return empty list on DB failure, not crash."""
        mock_db.query.side_effect = Exception("connection lost")

        result = context_manager._get_skill_definitions(4000)

        assert result == []


class TestGetCodeContext:
    """Tests for _get_code_context file path extraction."""

    def test_extracts_full_paths(self, context_manager):
        """Should extract full file paths, not just extensions."""
        events = [
            {"event_id": "e1", "content": "Check core/context/manager.py and src/index.ts"},
        ]
        result = context_manager._get_code_context(events, 2000)

        files = [r["file"] for r in result]
        assert "core/context/manager.py" in files
        assert "src/index.ts" in files

    def test_handles_complex_paths(self, context_manager):
        """Should handle hyphens, dots, and @ in paths."""
        events = [
            {"event_id": "e1", "content": "See my-app/src/App.tsx and @scope/pkg/index.js"},
        ]
        result = context_manager._get_code_context(events, 2000)

        files = [r["file"] for r in result]
        assert "my-app/src/App.tsx" in files
        assert "@scope/pkg/index.js" in files

    def test_deduplicates_paths(self, context_manager):
        """Same file mentioned twice should appear once."""
        events = [
            {"event_id": "e1", "content": "Fix main.py"},
            {"event_id": "e2", "content": "Also check main.py"},
        ]
        result = context_manager._get_code_context(events, 2000)

        assert len(result) == 1
        assert result[0]["file"] == "main.py"
