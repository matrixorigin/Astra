"""Advanced tests for edge cases and error handling.

Tests error conditions, boundary cases, and data integrity.
"""

import pytest
from uuid_utils import uuid7

from core.events.causal_chain import CausalChainManager
from core.events.event_logger import EventLogger
from core.events.event_reader import EventReader
from core.events.models import EventType
from core.events.session_manager import SessionManager
from sdk.database import Database


@pytest.fixture
def db():
    """Database fixture."""
    return Database()


@pytest.fixture
def event_logger(db):
    """Event logger fixture."""
    return EventLogger(db)


@pytest.fixture
def event_reader(db):
    """Event reader fixture."""
    return EventReader(db)


@pytest.fixture
def session_manager(db):
    """Session manager fixture."""
    return SessionManager(db)


@pytest.fixture
def causal_chain_manager(db):
    """Causal chain manager fixture."""
    return CausalChainManager(db)


def test_event_with_full_metadata(event_logger, event_reader):
    """Test event with complete metadata and nested structures."""
    user_id = f"test_user_{uuid7()}"
    session_id = f"test_session_{uuid7()}"

    # Create event with full metadata
    event = event_logger.create_llm_response(
        user_id=user_id,
        session_id=session_id,
        content="Complex response with metadata",
        agent_id="dev-agent",
        agent_version="0.1.0",
        parent_event_id=None,
        causal_chain_id=str(uuid7()),
        llm_model_used="gpt-4-turbo",
        token_usage={
            "prompt": 1500,
            "completion": 800,
            "total": 2300,
        },
        llm_params={
            "temperature": 0.7,
            "max_tokens": 2000,
            "top_p": 0.9,
            "frequency_penalty": 0.1,
        },
    )

    # Retrieve and verify all fields
    retrieved = event_reader.get_event(event.event_id)
    assert retrieved is not None
    assert retrieved.llm_model_used == "gpt-4-turbo"
    assert retrieved.token_usage.prompt == 1500
    assert retrieved.token_usage.completion == 800
    assert retrieved.token_usage.total == 2300
    assert retrieved.llm_params["temperature"] == 0.7
    assert retrieved.llm_params["max_tokens"] == 2000


def test_query_nonexistent_event(event_reader):
    """Test querying an event that doesn't exist."""
    result = event_reader.get_event("nonexistent_event_id")
    assert result is None


def test_query_nonexistent_session(event_reader):
    """Test querying a session that doesn't exist."""
    events = event_reader.get_session_events("nonexistent_session")
    assert events == []


def test_empty_causal_chain(causal_chain_manager):
    """Test querying an empty causal chain."""
    chain = causal_chain_manager.get_chain("nonexistent_chain")
    assert chain == []


def test_large_event_content(event_logger, event_reader):
    """Test event with large content (simulating long conversations)."""
    user_id = f"test_user_{uuid7()}"
    session_id = f"test_session_{uuid7()}"

    # Create event with large content (10KB)
    large_content = "x" * 10000

    event = event_logger.create_user_query(
        user_id=user_id,
        session_id=session_id,
        content=large_content,
    )

    # Verify retrieval
    retrieved = event_reader.get_event(event.event_id)
    assert retrieved is not None
    assert len(retrieved.content) == 10000
    assert retrieved.content == large_content


def test_long_causal_chain(event_logger, causal_chain_manager):
    """Test a long causal chain (10+ events)."""
    user_id = f"test_user_{uuid7()}"
    session_id = f"test_session_{uuid7()}"
    causal_chain_id = str(uuid7())

    # Create a chain of 10 events
    parent_id = None
    for i in range(10):
        event = event_logger.create_user_query(
            user_id=user_id,
            session_id=session_id,
            content=f"Query {i}",
            parent_event_id=parent_id,
            causal_chain_id=causal_chain_id,
        )
        parent_id = event.event_id

    # Verify chain
    chain = causal_chain_manager.get_chain(causal_chain_id)
    assert len(chain) == 10

    # Verify order
    for i, event in enumerate(chain):
        assert f"Query {i}" in event.content

    # Verify parent-child relationships
    for i in range(1, len(chain)):
        assert chain[i].parent_event_id == chain[i - 1].event_id


def test_chain_integrity_validation(event_logger, causal_chain_manager, db):
    """Test causal chain integrity validation with broken links."""
    user_id = f"test_user_{uuid7()}"
    session_id = f"test_session_{uuid7()}"
    causal_chain_id = str(uuid7())

    # Create valid chain
    event1 = event_logger.create_user_query(
        user_id=user_id,
        session_id=session_id,
        content="Event 1",
        causal_chain_id=causal_chain_id,
    )

    _event2 = event_logger.create_user_query(
        user_id=user_id,
        session_id=session_id,
        content="Event 2",
        parent_event_id=event1.event_id,
        causal_chain_id=causal_chain_id,
    )

    # Manually break the chain by referencing non-existent parent
    event3_id = str(uuid7())
    db.execute(
        """
        INSERT INTO conversation_events (
            event_id, user_id, session_id, agent_id, agent_version,
            event_type, content, parent_event_id, causal_chain_id
        ) VALUES (%s, %s, %s, %s, %s, %s, %s, %s, %s)
        """,
        (
            event3_id,
            user_id,
            session_id,
            "dev-agent",
            "0.1.0",
            EventType.USER_QUERY,
            "Event 3",
            "nonexistent_parent",
            causal_chain_id,
        ),
    )

    # Validate chain integrity
    validation = causal_chain_manager.validate_chain_integrity(causal_chain_id)
    assert validation["is_valid"] is False
    assert len(validation["issues"]) > 0
    assert "nonexistent_parent" in validation["issues"][0]


def test_session_event_count_accuracy(session_manager, event_logger):
    """Test that session event count is accurately maintained."""
    user_id = f"test_user_{uuid7()}"
    session = session_manager.create_session(user_id=user_id)

    # Create 5 events
    for i in range(5):
        event = event_logger.create_user_query(
            user_id=user_id,
            session_id=session.session_id,
            content=f"Query {i}",
        )
        session_manager.update_session_activity(session.session_id, event.event_id)

    # Verify count
    updated_session = session_manager.get_session(session.session_id)
    assert updated_session.event_count == 5


def test_concurrent_session_updates(session_manager, event_logger):
    """Test multiple rapid session updates."""
    user_id = f"test_user_{uuid7()}"
    session = session_manager.create_session(user_id=user_id)

    # Rapidly create and update
    event_ids = []
    for i in range(20):
        event = event_logger.create_user_query(
            user_id=user_id,
            session_id=session.session_id,
            content=f"Rapid query {i}",
        )
        event_ids.append(event.event_id)
        session_manager.update_session_activity(session.session_id, event.event_id)

    # Verify final state
    updated_session = session_manager.get_session(session.session_id)
    assert updated_session.event_count == 20
    assert updated_session.last_event_id == event_ids[-1]


def test_chain_summary_with_mixed_events(event_logger, causal_chain_manager):
    """Test chain summary with different event types."""
    user_id = f"test_user_{uuid7()}"
    session_id = f"test_session_{uuid7()}"
    causal_chain_id = str(uuid7())

    # Create mixed event types
    user_event = event_logger.create_user_query(
        user_id=user_id,
        session_id=session_id,
        content="User query",
        causal_chain_id=causal_chain_id,
    )

    llm_event1 = event_logger.create_llm_response(
        user_id=user_id,
        session_id=session_id,
        content="LLM response 1",
        agent_id="dev-agent",
        agent_version="0.1.0",
        parent_event_id=user_event.event_id,
        causal_chain_id=causal_chain_id,
        token_usage={"prompt": 100, "completion": 50, "total": 150},
    )

    _llm_event2 = event_logger.create_llm_response(
        user_id=user_id,
        session_id=session_id,
        content="LLM response 2",
        agent_id="dev-agent",
        agent_version="0.1.0",
        parent_event_id=llm_event1.event_id,
        causal_chain_id=causal_chain_id,
        token_usage={"prompt": 200, "completion": 100, "total": 300},
    )

    # Get summary
    summary = causal_chain_manager.get_chain_summary(causal_chain_id)

    assert summary["total_events"] == 3
    assert summary["total_tokens"] == 450  # 150 + 300
    assert summary["event_types"][EventType.USER_QUERY] == 1
    assert summary["event_types"][EventType.LLM_RESPONSE] == 2


def test_user_cross_session_events(event_logger, event_reader, session_manager):
    """Test querying user events across multiple sessions."""
    user_id = f"test_user_{uuid7()}"

    # Create 3 sessions with events
    for i in range(3):
        session = session_manager.create_session(user_id=user_id)
        for j in range(2):
            event_logger.create_user_query(
                user_id=user_id,
                session_id=session.session_id,
                content=f"Session {i} Query {j}",
            )

    # Query all user events
    user_events = event_reader.get_user_events(user_id)
    assert len(user_events) >= 6  # At least 6 events (3 sessions * 2 events)


def test_session_metadata(session_manager):
    """Test session with custom metadata."""
    user_id = f"test_user_{uuid7()}"
    metadata = {
        "source": "web",
        "device": "desktop",
        "tags": ["important", "test"],
        "custom_field": 123,
    }

    session = session_manager.create_session(
        user_id=user_id,
        metadata=metadata,
    )

    # Retrieve and verify metadata
    retrieved = session_manager.get_session(session.session_id)
    assert retrieved.metadata == metadata
    assert retrieved.metadata["source"] == "web"
    assert retrieved.metadata["tags"] == ["important", "test"]
