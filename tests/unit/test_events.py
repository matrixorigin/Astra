"""Unit tests for event system.

Tests event creation, logging, and retrieval.
"""

import pytest
from ulid import ULID

from core.events.event_logger import EventLogger
from core.events.event_reader import EventReader
from core.events.models import EventType
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


def test_create_user_query(event_logger, event_reader):
    """Test creating and retrieving a user query event."""
    user_id = f"test_user_{ULID()}"
    session_id = f"test_session_{ULID()}"
    content = "How do I implement event logging?"

    # Create event
    event = event_logger.create_user_query(user_id=user_id, session_id=session_id, content=content)

    assert event.event_id is not None
    assert event.user_id == user_id
    assert event.session_id == session_id
    assert event.content == content
    assert event.event_type == EventType.USER_QUERY
    assert event.causal_chain_id is not None

    # Retrieve event
    retrieved = event_reader.get_event(event.event_id)
    assert retrieved is not None
    assert retrieved.event_id == event.event_id
    assert retrieved.content == content


def test_create_llm_response(event_logger, event_reader):
    """Test creating an LLM response event."""
    user_id = f"test_user_{ULID()}"
    session_id = f"test_session_{ULID()}"
    causal_chain_id = str(ULID())

    # Create user query first
    user_event = event_logger.create_user_query(
        user_id=user_id,
        session_id=session_id,
        content="Test query",
        causal_chain_id=causal_chain_id,
    )

    # Create LLM response
    response_content = "Here's how to implement event logging..."
    token_usage = {"prompt": 100, "completion": 50, "total": 150}

    response_event = event_logger.create_llm_response(
        user_id=user_id,
        session_id=session_id,
        content=response_content,
        agent_id="dev-agent",
        agent_version="0.1.0",
        parent_event_id=user_event.event_id,
        causal_chain_id=causal_chain_id,
        llm_model_used="gpt-4",
        token_usage=token_usage,
        llm_params={"temperature": 0.7, "max_tokens": 1000},
    )

    assert response_event.event_type == EventType.LLM_RESPONSE
    assert response_event.parent_event_id == user_event.event_id
    assert response_event.causal_chain_id == causal_chain_id
    assert response_event.token_usage is not None
    assert response_event.token_usage.total == 150

    # Retrieve and verify
    retrieved = event_reader.get_event(response_event.event_id)
    assert retrieved is not None
    assert retrieved.llm_model_used == "gpt-4"
    assert retrieved.token_usage.prompt == 100


def test_get_session_events(event_logger, event_reader):
    """Test retrieving all events for a session."""
    user_id = f"test_user_{ULID()}"
    session_id = f"test_session_{ULID()}"

    # Create multiple events
    event1 = event_logger.create_user_query(
        user_id=user_id, session_id=session_id, content="Query 1"
    )
    event2 = event_logger.create_user_query(
        user_id=user_id, session_id=session_id, content="Query 2"
    )

    # Retrieve session events
    events = event_reader.get_session_events(session_id)
    assert len(events) >= 2

    event_ids = [e.event_id for e in events]
    assert event1.event_id in event_ids
    assert event2.event_id in event_ids


def test_get_causal_chain(event_logger, event_reader):
    """Test retrieving events in a causal chain."""
    user_id = f"test_user_{ULID()}"
    session_id = f"test_session_{ULID()}"
    causal_chain_id = str(ULID())

    # Create a causal chain
    user_event = event_logger.create_user_query(
        user_id=user_id,
        session_id=session_id,
        content="Test query",
        causal_chain_id=causal_chain_id,
    )

    response_event = event_logger.create_llm_response(
        user_id=user_id,
        session_id=session_id,
        content="Test response",
        agent_id="dev-agent",
        agent_version="0.1.0",
        parent_event_id=user_event.event_id,
        causal_chain_id=causal_chain_id,
    )

    # Retrieve causal chain
    chain = event_reader.get_causal_chain(causal_chain_id)
    assert len(chain) == 2
    assert chain[0].event_id == user_event.event_id
    assert chain[1].event_id == response_event.event_id
    assert chain[1].parent_event_id == chain[0].event_id
