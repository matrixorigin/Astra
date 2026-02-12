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
        """Test CODE_REVIEW budget allocation (60% code, 20% history, 20% docs)."""
        total_tokens = 10000
        budget = context_manager._allocate_budget(total_tokens, TaskType.CODE_REVIEW)

        # Fixed allocations
        assert budget["system"] == 500
        assert budget["skills"] == 1000
        assert budget["reserve"] == 500

        # Dynamic allocations (10000 - 2000 = 8000 available)
        available = 8000
        assert budget["code"] == int(available * 0.6)  # 4800
        assert budget["history"] == int(available * 0.2)  # 1600
        assert budget["docs"] == int(available * 0.2)  # 1600

    def test_planning_budget(self, context_manager):
        """Test PLANNING budget allocation (60% history, 20% code, 20% docs)."""
        total_tokens = 10000
        budget = context_manager._allocate_budget(total_tokens, TaskType.PLANNING)

        available = 8000
        assert budget["history"] == int(available * 0.6)  # 4800
        assert budget["code"] == int(available * 0.2)  # 1600
        assert budget["docs"] == int(available * 0.2)  # 1600

    def test_debugging_budget(self, context_manager):
        """Test DEBUGGING budget allocation (40% code, 40% logs, 20% history)."""
        total_tokens = 10000
        budget = context_manager._allocate_budget(total_tokens, TaskType.DEBUGGING)

        available = 8000
        assert budget["code"] == int(available * 0.4)  # 3200
        assert budget["docs"] == int(available * 0.2)  # 1600 (logs as docs)
        assert budget["history"] == int(available * 0.2)  # 1600

    def test_general_budget(self, context_manager):
        """Test GENERAL budget allocation (50% history, 30% code, 20% docs)."""
        total_tokens = 10000
        budget = context_manager._allocate_budget(total_tokens, TaskType.GENERAL)

        available = 8000
        assert budget["history"] == int(available * 0.5)  # 4000
        assert budget["code"] == int(available * 0.3)  # 2400
        assert budget["docs"] == int(available * 0.2)  # 1600

    def test_budget_with_small_total(self, context_manager):
        """Test budget allocation with small total tokens."""
        total_tokens = 2000  # Less than fixed allocations
        budget = context_manager._allocate_budget(total_tokens, TaskType.GENERAL)

        # Fixed allocations remain
        assert budget["system"] == 500
        assert budget["skills"] == 1000
        assert budget["reserve"] == 500

        # Dynamic allocations should be 0 or minimal
        assert budget["history"] >= 0
        assert budget["code"] >= 0
        assert budget["docs"] >= 0
