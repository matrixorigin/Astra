"""Unit tests for event operations with SQLAlchemy."""

from datetime import datetime, timezone
from uuid import uuid4

import pytest

from api.database import get_db_session
from api.repositories.session_repository import SessionRepository
from api.repositories.event_repository import EventRepository


@pytest.fixture
def db_session():
    session = next(get_db_session())
    yield session
    session.close()


@pytest.fixture
def session_repo(db_session):
    return SessionRepository(db_session)


@pytest.fixture
def event_repo(db_session):
    return EventRepository(db_session)


@pytest.fixture
def test_session(session_repo):
    """Create a test session."""
    return session_repo.create({
        "session_id": str(uuid4()),
        "user_id": str(uuid4()),
        "status": "active",
        "event_count": 0,
        "created_at": datetime.now(timezone.utc),
        "last_active_at": datetime.now(timezone.utc),
        "session_metadata": {},
    })


def test_create_user_query(event_repo, test_session):
    """Test creating a user query event."""
    event = event_repo.create({
        "event_id": str(uuid4()),
        "session_id": test_session.session_id,
        "user_id": test_session.user_id,
        "agent_id": "system",
        "agent_version": "1.0.0",
        "event_type": "user_query",
        "content": "Test query",
        "created_at": datetime.now(timezone.utc),
        "event_metadata": {},
        "causal_chain_id": str(uuid4()),
    })

    assert event.event_type == "user_query"
    assert event.content == "Test query"


def test_create_llm_response(event_repo, test_session):
    """Test creating an LLM response event."""
    event = event_repo.create({
        "event_id": str(uuid4()),
        "session_id": test_session.session_id,
        "user_id": test_session.user_id,
        "agent_id": "dev-agent",
        "agent_version": "0.1.0",
        "event_type": "llm_response",
        "content": "Test response",
        "created_at": datetime.now(timezone.utc),
        "event_metadata": {"model": "gpt-4"},
        "causal_chain_id": str(uuid4()),
    })

    assert event.event_type == "llm_response"
    assert event.event_metadata["model"] == "gpt-4"


def test_get_event_by_id(event_repo, test_session):
    """Test retrieving event by ID."""
    created = event_repo.create({
        "event_id": str(uuid4()),
        "session_id": test_session.session_id,
        "user_id": test_session.user_id,
        "agent_id": "system",
        "agent_version": "1.0.0",
        "event_type": "user_query",
        "content": "Test",
        "created_at": datetime.now(timezone.utc),
        "event_metadata": {},
        "causal_chain_id": str(uuid4()),
    })

    retrieved = event_repo.get_by_id(created.event_id, test_session.user_id)
    assert retrieved.event_id == created.event_id


def test_list_session_events(event_repo, test_session):
    """Test listing events for a session."""
    for i in range(3):
        event_repo.create({
            "event_id": str(uuid4()),
            "session_id": test_session.session_id,
            "user_id": test_session.user_id,
            "agent_id": "system",
            "agent_version": "1.0.0",
            "event_type": "user_query",
            "content": f"Query {i}",
            "created_at": datetime.now(timezone.utc),
            "event_metadata": {},
            "causal_chain_id": str(uuid4()),
        })

    events = event_repo.list_by_session(test_session.session_id, test_session.user_id)
    assert len(events) == 3


def test_event_with_parent(event_repo, test_session):
    """Test event with parent relationship."""
    parent = event_repo.create({
        "event_id": str(uuid4()),
        "session_id": test_session.session_id,
        "user_id": test_session.user_id,
        "agent_id": "system",
        "agent_version": "1.0.0",
        "event_type": "user_query",
        "content": "Parent",
        "created_at": datetime.now(timezone.utc),
        "event_metadata": {},
        "causal_chain_id": str(uuid4()),
    })

    child = event_repo.create({
        "event_id": str(uuid4()),
        "session_id": test_session.session_id,
        "user_id": test_session.user_id,
        "agent_id": "dev-agent",
        "agent_version": "0.1.0",
        "event_type": "llm_response",
        "content": "Child",
        "created_at": datetime.now(timezone.utc),
        "event_metadata": {},
        "parent_event_id": parent.event_id,
        "causal_chain_id": str(uuid4()),
    })

    assert child.parent_event_id == parent.event_id


def test_event_with_causal_chain(event_repo, test_session):
    """Test event with causal chain."""
    causal_chain_id = str(uuid4())

    event = event_repo.create({
        "event_id": str(uuid4()),
        "session_id": test_session.session_id,
        "user_id": test_session.user_id,
        "agent_id": "system",
        "agent_version": "1.0.0",
        "event_type": "user_query",
        "content": "Test",
        "created_at": datetime.now(timezone.utc),
        "event_metadata": {},
        "causal_chain_id": causal_chain_id,
    })

    assert event.causal_chain_id == causal_chain_id


def test_filter_events_by_type(event_repo, test_session):
    """Test filtering events by type."""
    event_repo.create({
        "event_id": str(uuid4()),
        "session_id": test_session.session_id,
        "user_id": test_session.user_id,
        "agent_id": "system",
        "agent_version": "1.0.0",
        "event_type": "user_query",
        "content": "Query",
        "created_at": datetime.now(timezone.utc),
        "event_metadata": {},
        "causal_chain_id": str(uuid4()),
    })

    event_repo.create({
        "event_id": str(uuid4()),
        "session_id": test_session.session_id,
        "user_id": test_session.user_id,
        "agent_id": "dev-agent",
        "agent_version": "0.1.0",
        "event_type": "llm_response",
        "content": "Response",
        "created_at": datetime.now(timezone.utc),
        "event_metadata": {},
        "causal_chain_id": str(uuid4()),
    })

    queries = event_repo.list_by_session(
        test_session.session_id,
        test_session.user_id,
        event_type="user_query"
    )
    assert len(queries) == 1
    assert queries[0].event_type == "user_query"
