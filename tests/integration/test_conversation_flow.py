"""Integration tests for session and event management with SQLAlchemy.

Tests complete conversation flows with sessions and event chains.
"""

from datetime import datetime, timezone
from uuid import uuid4

import pytest
from sqlalchemy.orm import Session

from api.database import get_db_session
from api.repositories.session_repository import SessionRepository
from api.repositories.event_repository import EventRepository


@pytest.fixture
def session_repo(db_session):
    """Session repository fixture."""
    return SessionRepository(lambda: db_session)


@pytest.fixture
def event_repo(db_session):
    """Event repository fixture."""
    return EventRepository(lambda: db_session)


def test_complete_conversation_flow(session_repo, event_repo, db_session):
    """Test a complete conversation flow with session and events."""
    user_id = str(uuid4())

    # 1. Create session
    session_data = {
        "session_id": str(uuid4()),
        "user_id": user_id,
        "status": "active",
        "event_count": 0,
        "created_at": datetime.now(timezone.utc),
        "last_active_at": datetime.now(timezone.utc),
        "session_metadata": {},
    }
    session = session_repo.create(session_data)
    assert session.session_id is not None
    assert session.status == "active"
    assert session.event_count == 0

    # 2. User asks a question
    causal_chain_id = str(uuid4())
    user_event_data = {
        "event_id": str(uuid4()),
        "session_id": session.session_id,
        "user_id": user_id,
        "agent_id": "system",
        "agent_version": "1.0.0",
        "event_type": "user_query",
        "content": "How do I implement event logging?",
        "created_at": datetime.now(timezone.utc),
        "event_metadata": {},
        "causal_chain_id": causal_chain_id,
    }
    user_event = event_repo.create(user_event_data)

    # Update session
    session.event_count += 1
    session.last_active_at = datetime.now(timezone.utc)
    db_session.commit()

    # 3. LLM responds
    llm_event_data = {
        "event_id": str(uuid4()),
        "session_id": session.session_id,
        "user_id": user_id,
        "agent_id": "dev-agent",
        "agent_version": "0.1.0",
        "event_type": "llm_response",
        "content": "Here's how to implement event logging...",
        "created_at": datetime.now(timezone.utc),
        "event_metadata": {"model": "gpt-4"},
        "parent_event_id": user_event.event_id,
        "causal_chain_id": causal_chain_id,
    }
    llm_event = event_repo.create(llm_event_data)

    # Update session
    session.event_count += 1
    db_session.commit()

    # 4. Verify session state
    updated_session = session_repo.get_by_id(session.session_id)
    assert updated_session.event_count == 2

    # 5. Verify events
    events = event_repo.list_by_session(session.session_id, user_id)
    assert len(events) == 2

    # Find events by type instead of assuming order
    user_events = [e for e in events if e.event_type == "user_query"]
    llm_events = [e for e in events if e.event_type == "llm_response"]

    assert len(user_events) == 1
    assert len(llm_events) == 1
    assert user_events[0].event_id == user_event.event_id
    assert llm_events[0].event_id == llm_event.event_id

    # 6. Verify causal chain
    assert user_events[0].causal_chain_id == causal_chain_id
    assert llm_events[0].causal_chain_id == causal_chain_id
    assert llm_events[0].parent_event_id == user_events[0].event_id


def test_multi_turn_conversation(session_repo, event_repo, db_session):
    """Test multi-turn conversation with multiple events."""
    user_id = str(uuid4())
    session_data = {
        "session_id": str(uuid4()),
        "user_id": user_id,
        "status": "active",
        "event_count": 0,
        "created_at": datetime.now(timezone.utc),
        "last_active_at": datetime.now(timezone.utc),
        "session_metadata": {},
    }
    session = session_repo.create(session_data)

    # Create 3 turns
    for i in range(3):
        causal_chain_id = str(uuid4())

        # User query
        user_event_data = {
            "event_id": str(uuid4()),
            "session_id": session.session_id,
            "user_id": user_id,
            "agent_id": "system",
            "agent_version": "1.0.0",
            "event_type": "user_query",
            "content": f"Question {i + 1}",
            "created_at": datetime.now(timezone.utc),
            "event_metadata": {},
            "causal_chain_id": causal_chain_id,
        }
        user_event = event_repo.create(user_event_data)

        # LLM response
        llm_event_data = {
            "event_id": str(uuid4()),
            "session_id": session.session_id,
            "user_id": user_id,
            "agent_id": "dev-agent",
            "agent_version": "0.1.0",
            "event_type": "llm_response",
            "content": f"Answer {i + 1}",
            "created_at": datetime.now(timezone.utc),
            "event_metadata": {},
            "parent_event_id": user_event.event_id,
            "causal_chain_id": causal_chain_id,
        }
        event_repo.create(llm_event_data)

    # Verify
    events = event_repo.list_by_session(session.session_id, user_id)
    assert len(events) == 6  # 3 turns * 2 events


def test_get_user_sessions(session_repo):
    """Test retrieving all sessions for a user."""
    user_id = str(uuid4())

    # Create 3 sessions
    for i in range(3):
        session_data = {
            "session_id": str(uuid4()),
            "user_id": user_id,
            "status": "active",
            "event_count": 0,
            "created_at": datetime.now(timezone.utc),
            "last_active_at": datetime.now(timezone.utc),
            "session_metadata": {"index": i},
        }
        session_repo.create(session_data)

    # Retrieve
    sessions, total = session_repo.list_by_user(user_id)
    assert len(sessions) == 3


def test_parent_child_relationships(event_repo, session_repo, db_session):
    """Test parent-child event relationships."""
    user_id = str(uuid4())
    session_data = {
        "session_id": str(uuid4()),
        "user_id": user_id,
        "status": "active",
        "event_count": 0,
        "created_at": datetime.now(timezone.utc),
        "last_active_at": datetime.now(timezone.utc),
        "session_metadata": {},
    }
    session = session_repo.create(session_data)

    causal_chain_id = str(uuid4())

    # Create parent event
    parent_data = {
        "event_id": str(uuid4()),
        "session_id": session.session_id,
        "user_id": user_id,
        "agent_id": "system",
        "agent_version": "1.0.0",
        "event_type": "user_query",
        "content": "Parent",
        "created_at": datetime.now(timezone.utc),
        "event_metadata": {},
        "causal_chain_id": causal_chain_id,
    }
    parent = event_repo.create(parent_data)

    # Create child event
    child_data = {
        "event_id": str(uuid4()),
        "session_id": session.session_id,
        "user_id": user_id,
        "agent_id": "dev-agent",
        "agent_version": "0.1.0",
        "event_type": "llm_response",
        "content": "Child",
        "created_at": datetime.now(timezone.utc),
        "event_metadata": {},
        "parent_event_id": parent.event_id,
        "causal_chain_id": causal_chain_id,
    }
    child = event_repo.create(child_data)

    # Verify relationship
    assert child.parent_event_id == parent.event_id
    assert child.causal_chain_id == parent.causal_chain_id
