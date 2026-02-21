"""Unit tests for ContextManager snapshot functionality."""

import json
from unittest.mock import MagicMock, patch

import pytest
from core.context.manager import Context, ContextManager, TaskType


@pytest.fixture
def mock_db():
    """Mock database connection."""
    db = MagicMock()
    return db


@pytest.fixture
def context_manager(mock_db):
    """Create ContextManager with mocked dependencies."""
    with patch("core.context.embeddings.EmbeddingService"):
        with patch("core.context.prompts.PromptManager"):
            with patch("core.context.scorer.RelevanceScorer"):
                cm = ContextManager(db=mock_db)
                yield cm


@pytest.fixture
def sample_context():
    """Create a sample Context object."""
    return Context(
        system_prompt="System prompt",
        skill_definitions=[{"skill_name": "skill1", "version": "v1"}, {"skill_name": "skill2"}],
        selected_events=[],
        code_context=[],
        documentation=[],
        total_tokens=100,
        token_budget={"total": 1000},
        assembly_time_ms=50,
        relevance_scores={},
        task_type=TaskType.GENERAL,
    )


class TestSaveSnapshot:
    """Tests for save_snapshot method."""

    def test_save_snapshot_basic(self, context_manager, mock_db, sample_context):
        """Test basic snapshot saving."""
        context_capture_id = context_manager.save_snapshot(
            context=sample_context,
            session_id="session-123",
            event_id="event-456"
        )

        assert context_capture_id is not None
        assert mock_db.add.called
        assert mock_db.commit.called
        
        # Verify snapshot object
        snapshot = mock_db.add.call_args[0][0]
        
        assert snapshot.session_id == "session-123"
        assert snapshot.event_id == "event-456"
        
        # Verify skills_used extraction
        skills_used = snapshot.skills_used
        assert len(skills_used) == 2
        assert skills_used[0]["skill_name"] == "skill1"
        assert skills_used[0]["version"] == "v1"
        assert skills_used[1]["skill_name"] == "skill2"
        assert skills_used[1]["version"] == "latest"  # Default fallback

    def test_save_snapshot_with_llm_ids(self, context_manager, mock_db, sample_context):
        """Test snapshot saving with LLM IDs."""
        context_manager.save_snapshot(
            context=sample_context,
            session_id="session-123",
            llm_request_id="req-1",
            llm_response_id="res-1"
        )
        
        snapshot = mock_db.add.call_args[0][0]
        
        assert snapshot.llm_request_id == "req-1"
        assert snapshot.llm_response_id == "res-1"


class TestUpdateSnapshotLlmIds:
    """Tests for update_snapshot_llm_ids method."""

    def test_update_snapshot_ids(self, context_manager, mock_db):
        """Test updating LLM IDs."""
        # Mock query chain
        mock_query = mock_db.query.return_value
        mock_filter = mock_query.filter.return_value
        
        context_manager.update_snapshot_llm_ids(
            context_capture_id="snap-123",
            llm_request_id="req-1",
            llm_response_id="res-1"
        )
        
        assert mock_db.query.called
        mock_filter.update.assert_called_once()
        update_dict = mock_filter.update.call_args[0][0]
        assert update_dict["llm_request_id"] == "req-1"
        assert update_dict["llm_response_id"] == "res-1"
        assert mock_db.commit.called

    def test_update_snapshot_ids_partial(self, context_manager, mock_db):
        """Test updating with only one ID."""
        # Mock query chain
        mock_query = mock_db.query.return_value
        mock_filter = mock_query.filter.return_value
        
        context_manager.update_snapshot_llm_ids(
            context_capture_id="snap-123",
            llm_request_id="req-1"
        )

        update_dict = mock_filter.update.call_args[0][0]
        assert "llm_request_id" in update_dict
        assert "llm_response_id" not in update_dict
