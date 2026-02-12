"""Test context snapshots functionality."""

import pytest

from core.context.manager import ContextManager, TaskType
from core.events.event_logger import EventLogger
from sdk import Database


@pytest.fixture
def db():
    """Database fixture."""
    return Database()


@pytest.fixture
def context_manager(db):
    """Context manager fixture."""
    return ContextManager(db, embedding_provider="mock")


@pytest.fixture
def event_logger(db):
    """Event logger fixture."""
    return EventLogger(db)


def test_context_snapshot_save_and_load(db, context_manager, event_logger):
    """Test saving and loading context snapshots."""
    # Create a session and event
    session_id = "test_session_001"
    user_id = "test_user"

    # Log a user query
    event = event_logger.create_user_query(
        user_id=user_id, session_id=session_id, content="Test query for context snapshot"
    )

    # Build context
    context = context_manager.build_context(
        session_id=session_id, query="Test query", task_type=TaskType.GENERAL
    )

    # Save snapshot (always returns snapshot_id)
    snapshot_id = context_manager.save_snapshot(context, session_id, event.event_id)
    assert snapshot_id is not None

    # Load snapshot
    loaded_context = context_manager.load_snapshot(snapshot_id)

    # Verify loaded context matches original
    assert loaded_context.system_prompt == context.system_prompt
    assert loaded_context.total_tokens == context.total_tokens
    assert loaded_context.task_type == context.task_type
    assert loaded_context.assembly_time_ms == context.assembly_time_ms

    # Verify snapshot is in database
    row = db.fetchone("SELECT * FROM context_snapshots WHERE snapshot_id = %s", (snapshot_id,))
    assert row is not None
    assert row["session_id"] == session_id
    assert row["event_id"] == event.event_id


def test_context_snapshot_with_events(db, context_manager, event_logger):
    """Test snapshot with actual conversation events."""
    session_id = "test_session_003"
    user_id = "test_user"

    # Create multiple events
    event1 = event_logger.create_user_query(
        user_id=user_id, session_id=session_id, content="First query"
    )

    event_logger.create_llm_response(
        user_id=user_id,
        session_id=session_id,
        content="First response",
        agent_id="test-agent",
        agent_version="1.0",
        parent_event_id=event1.event_id,
        causal_chain_id=event1.causal_chain_id,
    )

    event2 = event_logger.create_user_query(
        user_id=user_id, session_id=session_id, content="Second query"
    )

    # Build context (should include both events)
    context = context_manager.build_context(
        session_id=session_id, query="Second query", task_type=TaskType.GENERAL
    )

    # Save snapshot (always returns snapshot_id)
    snapshot_id = context_manager.save_snapshot(context, session_id, event2.event_id)
    assert snapshot_id is not None

    # Load and verify
    loaded = context_manager.load_snapshot(snapshot_id)
    assert len(loaded.selected_events) > 0


def test_context_snapshot_task_types(db, context_manager):
    """Test snapshots for different task types."""
    session_id = "test_session_004"

    task_types = [
        TaskType.CODE_REVIEW,
        TaskType.PLANNING,
        TaskType.DEBUGGING,
        TaskType.GENERAL,
    ]

    for task_type in task_types:
        context = context_manager.build_context(
            session_id=session_id, query=f"Test {task_type.value}", task_type=task_type
        )

        snapshot_id = context_manager.save_snapshot(context, session_id)
        assert snapshot_id is not None

        loaded = context_manager.load_snapshot(snapshot_id)
        assert loaded.task_type == task_type


def test_context_snapshot_relevance_scores(db, context_manager, event_logger):
    """Test that relevance scores are preserved in snapshots."""
    session_id = "test_session_005"
    user_id = "test_user"

    # Create events
    event = event_logger.create_user_query(
        user_id=user_id, session_id=session_id, content="Query with relevance"
    )

    # Build context
    context = context_manager.build_context(
        session_id=session_id, query="Query with relevance", task_type=TaskType.GENERAL
    )

    # Save snapshot
    snapshot_id = context_manager.save_snapshot(context, session_id, event.event_id)

    # Load and verify relevance scores
    loaded = context_manager.load_snapshot(snapshot_id)
    assert loaded.relevance_scores is not None
    assert isinstance(loaded.relevance_scores, dict)


if __name__ == "__main__":
    pytest.main([__file__, "-v"])
