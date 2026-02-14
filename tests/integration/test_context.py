"""Tests for Context management with SQLAlchemy."""

import pytest
from uuid import uuid4

from api.database import get_db_session
from api.repositories.session_repository import SessionRepository
from api.repositories.event_repository import EventRepository


@pytest.fixture
def session_repo(db_session):
    """Session repository fixture."""
    return SessionRepository(db_session)


@pytest.fixture
def event_repo(db_session):
    """Event repository fixture."""
    return EventRepository(db_session)


def test_context_manager_empty_session(session_repo, event_repo):
    """Test context manager with empty session."""
    user_id = str(uuid4())
    session_id = str(uuid4())
    
    # Create session
    session = session_repo.create({
        "session_id": session_id,
        "user_id": user_id
    })
    
    # Verify session exists
    retrieved = session_repo.get_by_id(session.session_id)
    assert retrieved is not None
    assert retrieved.user_id == user_id
    assert retrieved.event_count == 0


def test_context_with_events(session_repo, event_repo):
    """Test context with events."""
    user_id = str(uuid4())
    session_id = str(uuid4())
    
    # Create session
    session = session_repo.create({
        "session_id": session_id,
        "user_id": user_id
    })
    
    # Create events
    event1_id = str(uuid4())
    causal_chain_id = str(uuid4())
    
    event1 = event_repo.create({
        "event_id": event1_id,
        "user_id": user_id,
        "session_id": session.session_id,
        "event_type": "user_query",
        "content": "Test query 1",
        "causal_chain_id": causal_chain_id
    })
    
    event2 = event_repo.create({
        "event_id": str(uuid4()),
        "user_id": user_id,
        "session_id": session.session_id,
        "event_type": "llm_response",
        "content": "Test response 1",
        "parent_event_id": event1.event_id,
        "causal_chain_id": event1.causal_chain_id
    })
    
    # Verify events
    events = event_repo.list_by_session(session.session_id, user_id)
    assert len(events) == 2
    # Events might be in different order, so check both exist
    contents = [e.content for e in events]
    assert "Test query 1" in contents
    assert "Test response 1" in contents
    # Check parent relationship
    response_event = next(e for e in events if e.content == "Test response 1")
    assert response_event.parent_event_id == event1.event_id


def test_context_cross_session(session_repo, event_repo):
    """Test context across multiple sessions."""
    user_id = str(uuid4())
    
    # Create two sessions
    session1 = session_repo.create({
        "session_id": str(uuid4()),
        "user_id": user_id
    })
    session2 = session_repo.create({
        "session_id": str(uuid4()),
        "user_id": user_id
    })
    
    # Create events in each session
    event1 = event_repo.create({
        "event_id": str(uuid4()),
        "user_id": user_id,
        "session_id": session1.session_id,
        "event_type": "user_query",
        "content": "Query in session 1",
        "causal_chain_id": str(uuid4())
    })
    
    event2 = event_repo.create({
        "event_id": str(uuid4()),
        "user_id": user_id,
        "session_id": session2.session_id,
        "event_type": "user_query",
        "content": "Query in session 2",
        "causal_chain_id": str(uuid4())
    })
    
    # Verify events are in correct sessions
    events1 = event_repo.list_by_session(session1.session_id, user_id)
    events2 = event_repo.list_by_session(session2.session_id, user_id)
    
    assert len(events1) == 1
    assert len(events2) == 1
    assert events1[0].content == "Query in session 1"
    assert events2[0].content == "Query in session 2"


def test_context_causal_chains(event_repo):
    """Test causal chain tracking."""
    user_id = str(uuid4())
    session_id = str(uuid4())
    causal_chain_id = str(uuid4())
    
    # Create initial event
    event1 = event_repo.create({
        "event_id": str(uuid4()),
        "user_id": user_id,
        "session_id": session_id,
        "event_type": "user_query",
        "content": "Initial query",
        "causal_chain_id": causal_chain_id
    })
    
    # Create response in same chain
    event2 = event_repo.create({
        "event_id": str(uuid4()),
        "user_id": user_id,
        "session_id": session_id,
        "event_type": "llm_response",
        "content": "Response to query",
        "parent_event_id": event1.event_id,
        "causal_chain_id": causal_chain_id
    })
    
    # Verify events exist and have correct chain
    retrieved1 = event_repo.get_by_id(event1.event_id)
    retrieved2 = event_repo.get_by_id(event2.event_id)
    
    assert retrieved1.causal_chain_id == causal_chain_id
    assert retrieved2.causal_chain_id == causal_chain_id
    assert retrieved2.parent_event_id == event1.event_id
