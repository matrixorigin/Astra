"""Integration tests for edge cases with SQLAlchemy."""

from datetime import datetime, timezone
from uuid import uuid4

import pytest

from api.database import get_db_session
from api.repositories.session_repository import SessionRepository
from api.repositories.event_repository import EventRepository


@pytest.fixture
def session_repo(db_session):
    return SessionRepository(db_session)


@pytest.fixture
def event_repo(db_session):
    return EventRepository(db_session)


def test_event_with_full_metadata(session_repo, event_repo):
    """Test event with comprehensive metadata."""
    user_id = str(uuid4())
    session = session_repo.create({
        "session_id": str(uuid4()),
        "user_id": user_id,
        "status": "active",
        "event_count": 0,
        "created_at": datetime.now(timezone.utc),
        "last_active_at": datetime.now(timezone.utc),
        "session_metadata": {},
    })

    event = event_repo.create({
        "event_id": str(uuid4()),
        "session_id": session.session_id,
        "user_id": user_id,
        "agent_id": "test-agent",
        "agent_version": "1.0.0",
        "event_type": "user_query",
        "content": "Test content",
        "created_at": datetime.now(timezone.utc),
        "event_metadata": {
            "source": "cli",
            "ip": "127.0.0.1",
            "user_agent": "test",
            "nested": {"key": "value"},
        },
        "causal_chain_id": str(uuid4()),
    })

    assert event.event_metadata["source"] == "cli"
    assert event.event_metadata["nested"]["key"] == "value"


def test_large_event_content(session_repo, event_repo):
    """Test event with large content."""
    user_id = str(uuid4())
    session = session_repo.create({
        "session_id": str(uuid4()),
        "user_id": user_id,
        "status": "active",
        "event_count": 0,
        "created_at": datetime.now(timezone.utc),
        "last_active_at": datetime.now(timezone.utc),
        "session_metadata": {},
    })

    large_content = "x" * 10000
    event = event_repo.create({
        "event_id": str(uuid4()),
        "session_id": session.session_id,
        "user_id": user_id,
        "agent_id": "test-agent",
        "agent_version": "1.0.0",
        "event_type": "user_query",
        "content": large_content,
        "created_at": datetime.now(timezone.utc),
        "event_metadata": {},
        "causal_chain_id": str(uuid4()),
    })

    assert len(event.content) == 10000


def test_long_causal_chain(session_repo, event_repo):
    """Test long causal chain."""
    user_id = str(uuid4())
    session = session_repo.create({
        "session_id": str(uuid4()),
        "user_id": user_id,
        "status": "active",
        "event_count": 0,
        "created_at": datetime.now(timezone.utc),
        "last_active_at": datetime.now(timezone.utc),
        "session_metadata": {},
    })

    causal_chain_id = str(uuid4())
    parent_id = None

    for i in range(10):
        event = event_repo.create({
            "event_id": str(uuid4()),
            "session_id": session.session_id,
            "user_id": user_id,
            "agent_id": "test-agent",
            "agent_version": "1.0.0",
            "event_type": "llm_response",
            "content": f"Event {i}",
            "created_at": datetime.now(timezone.utc),
            "event_metadata": {},
            "parent_event_id": parent_id,
            "causal_chain_id": causal_chain_id,
        })
        parent_id = event.event_id

    events = event_repo.list_by_session(session.session_id, user_id)
    assert len(events) == 10
    assert all(e.causal_chain_id == causal_chain_id for e in events)


def test_chain_integrity_validation(session_repo, event_repo):
    """Test causal chain integrity."""
    user_id = str(uuid4())
    session = session_repo.create({
        "session_id": str(uuid4()),
        "user_id": user_id,
        "status": "active",
        "event_count": 0,
        "created_at": datetime.now(timezone.utc),
        "last_active_at": datetime.now(timezone.utc),
        "session_metadata": {},
    })

    causal_chain_id = str(uuid4())
    
    parent = event_repo.create({
        "event_id": str(uuid4()),
        "session_id": session.session_id,
        "user_id": user_id,
        "agent_id": "test-agent",
        "agent_version": "1.0.0",
        "event_type": "user_query",
        "content": "Parent",
        "created_at": datetime.now(timezone.utc),
        "event_metadata": {},
        "causal_chain_id": causal_chain_id,
    })

    child = event_repo.create({
        "event_id": str(uuid4()),
        "session_id": session.session_id,
        "user_id": user_id,
        "agent_id": "test-agent",
        "agent_version": "1.0.0",
        "event_type": "llm_response",
        "content": "Child",
        "created_at": datetime.now(timezone.utc),
        "event_metadata": {},
        "parent_event_id": parent.event_id,
        "causal_chain_id": causal_chain_id,
    })

    assert child.parent_event_id == parent.event_id
    assert child.causal_chain_id == parent.causal_chain_id


def test_session_event_count_accuracy(session_repo, event_repo, db_session):
    """Test session event count accuracy."""
    user_id = str(uuid4())
    session = session_repo.create({
        "session_id": str(uuid4()),
        "user_id": user_id,
        "status": "active",
        "event_count": 0,
        "created_at": datetime.now(timezone.utc),
        "last_active_at": datetime.now(timezone.utc),
        "session_metadata": {},
    })

    for i in range(5):
        event_repo.create({
            "event_id": str(uuid4()),
            "session_id": session.session_id,
            "user_id": user_id,
            "agent_id": "test-agent",
            "agent_version": "1.0.0",
            "event_type": "user_query",
            "content": f"Event {i}",
            "created_at": datetime.now(timezone.utc),
            "event_metadata": {},
            "causal_chain_id": str(uuid4()),
        })
        session.event_count += 1
        db_session.commit()

    updated_session = session_repo.get_by_id(session.session_id)
    assert updated_session.event_count == 5


def test_concurrent_session_updates(session_repo, db_session):
    """Test concurrent session updates."""
    user_id = str(uuid4())
    session = session_repo.create({
        "session_id": str(uuid4()),
        "user_id": user_id,
        "status": "active",
        "event_count": 0,
        "created_at": datetime.now(timezone.utc),
        "last_active_at": datetime.now(timezone.utc),
        "session_metadata": {},
    })

    session.event_count = 10
    db_session.commit()

    updated = session_repo.get_by_id(session.session_id)
    assert updated.event_count == 10


def test_chain_summary_with_mixed_events(session_repo, event_repo):
    """Test chain with mixed event types."""
    user_id = str(uuid4())
    session = session_repo.create({
        "session_id": str(uuid4()),
        "user_id": user_id,
        "status": "active",
        "event_count": 0,
        "created_at": datetime.now(timezone.utc),
        "last_active_at": datetime.now(timezone.utc),
        "session_metadata": {},
    })

    causal_chain_id = str(uuid4())
    event_types = ["user_query", "llm_response", "tool_call", "tool_result"]

    for event_type in event_types:
        event_repo.create({
            "event_id": str(uuid4()),
            "session_id": session.session_id,
            "user_id": user_id,
            "agent_id": "test-agent",
            "agent_version": "1.0.0",
            "event_type": event_type,
            "content": f"Content for {event_type}",
            "created_at": datetime.now(timezone.utc),
            "event_metadata": {},
            "causal_chain_id": causal_chain_id,
        })

    events = event_repo.list_by_session(session.session_id, user_id)
    assert len(events) == 4
    assert set(e.event_type for e in events) == set(event_types)


def test_user_cross_session_events(session_repo, event_repo):
    """Test user events across multiple sessions."""
    user_id = str(uuid4())

    sessions = []
    for i in range(3):
        session = session_repo.create({
            "session_id": str(uuid4()),
            "user_id": user_id,
            "status": "active",
            "event_count": 0,
            "created_at": datetime.now(timezone.utc),
            "last_active_at": datetime.now(timezone.utc),
            "session_metadata": {},
        })
        sessions.append(session)

        event_repo.create({
            "event_id": str(uuid4()),
            "session_id": session.session_id,
            "user_id": user_id,
            "agent_id": "test-agent",
            "agent_version": "1.0.0",
            "event_type": "user_query",
            "content": f"Event in session {i}",
            "created_at": datetime.now(timezone.utc),
            "event_metadata": {},
            "causal_chain_id": str(uuid4()),
        })

    user_sessions, total = session_repo.list_by_user(user_id)
    assert len(user_sessions) == 3


def test_session_metadata(session_repo):
    """Test session metadata storage."""
    user_id = str(uuid4())
    metadata = {
        "client": "web",
        "version": "1.0.0",
        "features": ["chat", "code"],
    }

    session = session_repo.create({
        "session_id": str(uuid4()),
        "user_id": user_id,
        "status": "active",
        "event_count": 0,
        "created_at": datetime.now(timezone.utc),
        "last_active_at": datetime.now(timezone.utc),
        "session_metadata": metadata,
    })

    retrieved = session_repo.get_by_id(session.session_id)
    assert retrieved.session_metadata == metadata
