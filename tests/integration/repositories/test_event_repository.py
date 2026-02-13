"""Integration tests for EventRepository with real database."""

import pytest
from sqlalchemy.orm import Session
from uuid import uuid4
from datetime import datetime, timezone

from api.database import get_db_session
from api.repositories.event_repository import EventRepository
from api.models import Event as EventModel


@pytest.fixture
def db_session():
    """SQLAlchemy Session fixture."""
    session = next(get_db_session())
    yield session
    session.close()


@pytest.fixture
def event_repo(db_session: Session):
    """Create EventRepository with real database session."""
    return EventRepository(db_session)


class TestEventRepositoryIntegration:
    """Integration tests for EventRepository with real database."""

    def test_create_and_retrieve_event(self, event_repo, db_session):
        """Test creating and retrieving an event."""
        event_data = {
            "event_id": str(uuid4()),
            "session_id": str(uuid4()),
            "user_id": str(uuid4()),
            "event_type": "user_query",
            "content": "Hello, world!",
            "created_at": datetime.now(timezone.utc),
            "event_metadata": {"client": "test"}
        }
        
        # Create
        event = event_repo.create(event_data)
        assert event.event_id == event_data["event_id"]
        assert event.content == "Hello, world!"
        
        # Retrieve
        retrieved = event_repo.get_by_id(event.event_id)
        assert retrieved is not None
        assert retrieved.event_id == event.event_id

    def test_get_by_id_with_user_filter(self, event_repo):
        """Test user filter in get_by_id."""
        user_id = str(uuid4())
        other_user = str(uuid4())
        
        event_data = {
            "event_id": str(uuid4()),
            "session_id": str(uuid4()),
            "user_id": user_id,
            "event_type": "user_query",
            "content": "Test",
            "created_at": datetime.now(timezone.utc)
        }
        event = event_repo.create(event_data)
        
        # Should find with correct user
        found = event_repo.get_by_id(event.event_id, user_id)
        assert found is not None
        
        # Should not find with wrong user
        not_found = event_repo.get_by_id(event.event_id, other_user)
        assert not_found is None

    def test_list_by_session(self, event_repo):
        """Test listing events by session."""
        session_id = str(uuid4())
        user_id = str(uuid4())
        
        # Create 3 events
        for i in range(3):
            event_data = {
                "event_id": str(uuid4()),
                "session_id": session_id,
                "user_id": user_id,
                "event_type": "user_query",
                "content": f"Message {i}",
                "created_at": datetime.now(timezone.utc)
            }
            event_repo.create(event_data)
        
        # List
        events = event_repo.list_by_session(session_id, user_id)
        assert len(events) == 3

    def test_list_by_session_with_filter(self, event_repo):
        """Test filtering by event_type."""
        session_id = str(uuid4())
        user_id = str(uuid4())
        
        # Create mixed types
        for event_type in ["user_query", "llm_response", "user_query"]:
            event_data = {
                "event_id": str(uuid4()),
                "session_id": session_id,
                "user_id": user_id,
                "event_type": event_type,
                "content": "Test",
                "created_at": datetime.now(timezone.utc)
            }
            event_repo.create(event_data)
        
        # Filter
        queries = event_repo.list_by_session(session_id, user_id, event_type="user_query")
        assert len(queries) == 2
        assert all(e.event_type == "user_query" for e in queries)

    def test_count_by_session(self, event_repo):
        """Test counting events."""
        session_id = str(uuid4())
        user_id = str(uuid4())
        
        for i in range(5):
            event_data = {
                "event_id": str(uuid4()),
                "session_id": session_id,
                "user_id": user_id,
                "event_type": "user_query",
                "content": f"Message {i}",
                "created_at": datetime.now(timezone.utc)
            }
            event_repo.create(event_data)
        
        count = event_repo.count_by_session(session_id)
        assert count == 5

    def test_get_by_user(self, event_repo):
        """Test getting events by user."""
        user_id = str(uuid4())
        
        for i in range(3):
            event_data = {
                "event_id": str(uuid4()),
                "session_id": str(uuid4()),
                "user_id": user_id,
                "event_type": "user_query",
                "content": f"Message {i}",
                "created_at": datetime.now(timezone.utc)
            }
            event_repo.create(event_data)
        
        events, total = event_repo.get_by_user(user_id)
        assert len(events) == 3
        assert total == 3

    def test_get_by_user_with_filters(self, event_repo):
        """Test get_by_user with all filters."""
        user_id = str(uuid4())
        session_id = str(uuid4())
        agent_id = str(uuid4())
        causal_chain_id = str(uuid4())
        
        event_data = {
            "event_id": str(uuid4()),
            "session_id": session_id,
            "user_id": user_id,
            "event_type": "llm_response",
            "content": "Test",
            "agent_id": agent_id,
            "causal_chain_id": causal_chain_id,
            "created_at": datetime.now(timezone.utc)
        }
        event_repo.create(event_data)
        
        events, total = event_repo.get_by_user(
            user_id, session_id=session_id, event_type="llm_response",
            agent_id=agent_id, causal_chain_id=causal_chain_id
        )
        assert total == 1
        assert len(events) == 1

    def test_get_by_causal_chain(self, event_repo):
        """Test getting events by causal chain."""
        causal_chain_id = str(uuid4())
        user_id = str(uuid4())
        
        for i in range(2):
            event_data = {
                "event_id": str(uuid4()),
                "session_id": str(uuid4()),
                "user_id": user_id,
                "event_type": "user_query",
                "content": f"Message {i}",
                "causal_chain_id": causal_chain_id,
                "created_at": datetime.now(timezone.utc)
            }
            event_repo.create(event_data)
        
        events = event_repo.get_by_causal_chain(causal_chain_id, user_id)
        assert len(events) == 2
        assert all(e.causal_chain_id == causal_chain_id for e in events)

    def test_get_by_session_paginated(self, event_repo):
        """Test pagination."""
        session_id = str(uuid4())
        
        for i in range(5):
            event_data = {
                "event_id": str(uuid4()),
                "session_id": session_id,
                "user_id": str(uuid4()),
                "event_type": "user_query",
                "content": f"Message {i}",
                "created_at": datetime.now(timezone.utc)
            }
            event_repo.create(event_data)
        
        page1, total = event_repo.get_by_session(session_id, limit=2, offset=0)
        assert len(page1) == 2
        assert total == 5
        
        page2, _ = event_repo.get_by_session(session_id, limit=2, offset=2)
        assert len(page2) == 2

    def test_delete_event(self, event_repo):
        """Test deleting an event."""
        event_data = {
            "event_id": str(uuid4()),
            "session_id": str(uuid4()),
            "user_id": str(uuid4()),
            "event_type": "user_query",
            "content": "To delete",
            "created_at": datetime.now(timezone.utc)
        }
        event = event_repo.create(event_data)
        
        # Delete
        result = event_repo.delete(event.event_id)
        assert result is True
        
        # Verify deleted
        deleted = event_repo.get_by_id(event.event_id)
        assert deleted is None

    def test_delete_nonexistent(self, event_repo):
        """Test deleting non-existent event."""
        result = event_repo.delete(str(uuid4()))
        assert result is False
