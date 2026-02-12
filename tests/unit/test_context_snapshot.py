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
    with patch("core.context.manager.get_cache"):
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
        skill_definitions=[{"name": "skill1", "version": "v1"}, {"name": "skill2"}],
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
        snapshot_id = context_manager.save_snapshot(
            context=sample_context,
            session_id="session-123",
            event_id="event-456"
        )

        assert snapshot_id is not None
        assert mock_db.execute.called
        
        # Verify SQL args
        args = mock_db.execute.call_args[0]
        query = args[0]
        params = args[1]
        
        assert "INSERT INTO context_snapshots" in query
        assert params[1] == "session-123"  # session_id
        assert params[2] == "event-456"    # event_id
        
        # Verify skills_used extraction
        skills_used_json = params[5]
        skills_used = json.loads(skills_used_json)
        assert len(skills_used) == 2
        assert skills_used[0]["name"] == "skill1"
        assert skills_used[0]["version"] == "v1"
        assert skills_used[1]["name"] == "skill2"
        assert skills_used[1]["version"] == "latest"  # Default fallback

    def test_save_snapshot_with_llm_ids(self, context_manager, mock_db, sample_context):
        """Test snapshot saving with LLM IDs."""
        context_manager.save_snapshot(
            context=sample_context,
            session_id="session-123",
            llm_request_id="req-1",
            llm_response_id="res-1"
        )
        
        args = mock_db.execute.call_args[0]
        params = args[1]
        
        assert params[14] == "req-1"  # llm_request_id
        assert params[15] == "res-1"  # llm_response_id


class TestUpdateSnapshotLlmIds:
    """Tests for update_snapshot_llm_ids method."""

    def test_update_snapshot_ids(self, context_manager, mock_db):
        """Test updating LLM IDs."""
        context_manager.update_snapshot_llm_ids(
            snapshot_id="snap-123",
            llm_request_id="req-1",
            llm_response_id="res-1"
        )
        
        assert mock_db.execute.called
        args = mock_db.execute.call_args[0]
        query = args[0]
        params = args[1]
        
        assert "UPDATE context_snapshots" in query
        assert "llm_request_id = %s" in query
        assert "llm_response_id = %s" in query
        assert params == ("req-1", "res-1", "snap-123")

    def test_update_snapshot_ids_partial(self, context_manager, mock_db):
        """Test updating with only one ID."""
        context_manager.update_snapshot_llm_ids(
            snapshot_id="snap-123",
            llm_request_id="req-1"
        )
        
        args = mock_db.execute.call_args[0]
        params = args[1]
        query = args[0]

        assert "llm_request_id = %s" in query
        assert "llm_response_id" not in query
        assert params == ("req-1", "snap-123")
