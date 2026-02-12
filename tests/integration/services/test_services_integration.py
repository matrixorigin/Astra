"""Integration tests for all services with real database."""

import pytest
from sqlalchemy.orm import Session
from uuid import uuid4
from datetime import datetime, timezone

from api.database import get_db_session
from api.services.agent_service import AgentService
from api.services.session_service import SessionService
from api.services.event_service import EventService


@pytest.fixture
def db_session():
    """Database session fixture."""
    session = next(get_db_session())
    yield session
    session.close()


@pytest.fixture
def agent_service(db_session: Session):
    return AgentService(db_session)


@pytest.fixture
def session_service(db_session: Session):
    return SessionService(db_session)


@pytest.fixture
def event_service(db_session: Session):
    return EventService(db_session)


class TestAgentService:
    """Integration tests for AgentService."""

    def test_create_and_get_agent(self, agent_service):
        """Test creating and retrieving an agent."""
        user_id = str(uuid4())
        
        agent = agent_service.create_agent(
            user_id=user_id,
            name="Test Agent",
            agent_config={"model": "gpt-4"}
        )
        
        assert agent["name"] == "Test Agent"
        assert agent["owner_user_id"] == user_id
        
        retrieved = agent_service.get_agent(agent["agent_id"], user_id)
        assert retrieved["agent_id"] == agent["agent_id"]

    def test_list_agents(self, agent_service):
        """Test listing agents."""
        user_id = str(uuid4())
        
        for i in range(3):
            agent_service.create_agent(user_id=user_id, name=f"Agent {i}")
        
        agents = agent_service.list_agents(user_id)
        assert len(agents) == 3

    def test_update_agent(self, agent_service):
        """Test updating an agent."""
        user_id = str(uuid4())
        agent = agent_service.create_agent(user_id=user_id, name="Original")
        
        updated = agent_service.update_agent(
            agent["agent_id"], user_id, name="Updated"
        )
        assert updated["name"] == "Updated"

    def test_delete_agent(self, agent_service):
        """Test deleting an agent."""
        user_id = str(uuid4())
        agent = agent_service.create_agent(user_id=user_id, name="To Delete")
        
        agent_service.delete_agent(agent["agent_id"], user_id)
        
        with pytest.raises(ValueError):
            agent_service.get_agent(agent["agent_id"], user_id)


class TestSessionService:
    """Integration tests for SessionService."""

    def test_create_and_get_session(self, session_service):
        """Test creating and retrieving a session."""
        user_id = str(uuid4())
        
        session = session_service.create_session(user_id=user_id)
        assert session["user_id"] == user_id
        
        retrieved = session_service.get_session(session["session_id"], user_id)
        assert retrieved["session_id"] == session["session_id"]

    def test_list_sessions(self, session_service):
        """Test listing sessions."""
        user_id = str(uuid4())
        
        for i in range(3):
            session_service.create_session(user_id=user_id)
        
        result = session_service.list_sessions(user_id)
        assert len(result["sessions"]) == 3

    def test_update_session(self, session_service):
        """Test updating a session."""
        user_id = str(uuid4())
        session = session_service.create_session(user_id=user_id, title="Original")
        
        updated = session_service.update_session(
            session["session_id"], user_id, title="Updated"
        )
        assert updated["title"] == "Updated"

    def test_delete_session(self, session_service):
        """Test deleting a session."""
        user_id = str(uuid4())
        session = session_service.create_session(user_id=user_id)
        
        session_service.delete_session(session["session_id"], user_id)
        
        with pytest.raises(ValueError):
            session_service.get_session(session["session_id"], user_id)
    
    def test_create_session_with_metadata(self, session_service):
        """Test creating session with metadata."""
        user_id = str(uuid4())
        metadata = {"key": "value", "nested": {"data": 123}}
        
        session = session_service.create_session(
            user_id=user_id,
            title="Test",
            metadata=metadata
        )
        
        assert session["metadata"] == metadata
        
        retrieved = session_service.get_session(session["session_id"], user_id)
        assert retrieved["metadata"] == metadata
    
    def test_list_sessions_with_filters(self, session_service, agent_service):
        """Test listing sessions with agent_id filter."""
        user_id = str(uuid4())
        agent = agent_service.create_agent(
            user_id=user_id,
            name="Test Agent",
            agent_config={"model": "gpt-4"}
        )
        
        session1 = session_service.create_session(user_id=user_id, agent_id=agent["agent_id"])
        session2 = session_service.create_session(user_id=user_id)
        
        result = session_service.list_sessions(user_id, agent_id=agent["agent_id"])
        assert len(result["sessions"]) == 1
        assert result["sessions"][0]["session_id"] == session1["session_id"]
    
    def test_get_session_permission_denied(self, session_service):
        """Test permission denied when accessing other user's session."""
        user_id = str(uuid4())
        other_user_id = str(uuid4())
        
        session = session_service.create_session(user_id=user_id)
        
        with pytest.raises(ValueError):
            session_service.get_session(session["session_id"], other_user_id)
    
    def test_update_session_permission_denied(self, session_service):
        """Test permission denied when updating other user's session."""
        user_id = str(uuid4())
        other_user_id = str(uuid4())
        
        session = session_service.create_session(user_id=user_id)
        
        with pytest.raises(ValueError):
            session_service.update_session(session["session_id"], other_user_id, title="Updated")
    
    def test_delete_session_permission_denied(self, session_service):
        """Test permission denied when deleting other user's session."""
        user_id = str(uuid4())
        other_user_id = str(uuid4())
        
        session = session_service.create_session(user_id=user_id)
        
        with pytest.raises(ValueError):
            session_service.delete_session(session["session_id"], other_user_id)


class TestEventService:
    """Integration tests for EventService."""

    def test_create_and_get_event(self, event_service, session_service):
        """Test creating and retrieving an event."""
        user_id = str(uuid4())
        session = session_service.create_session(user_id=user_id)
        
        event = event_service.create_event(
            user_id=user_id,
            session_id=session["session_id"],
            event_type="user_query",
            content="Hello"
        )
        
        assert event["content"] == "Hello"
        
        retrieved = event_service.get_event(event["event_id"], user_id)
        assert retrieved["event_id"] == event["event_id"]

    def test_list_events_by_session(self, event_service, session_service):
        """Test listing events by session."""
        user_id = str(uuid4())
        session = session_service.create_session(user_id=user_id)
        
        for i in range(3):
            event_service.create_event(
                user_id=user_id,
                session_id=session["session_id"],
                event_type="user_query",
                content=f"Message {i}"
            )
        
        result = event_service.get_session_events(session["session_id"], user_id)
        assert len(result["events"]) == 3

    def test_list_events_by_user(self, event_service, session_service):
        """Test listing events by user."""
        user_id = str(uuid4())
        session = session_service.create_session(user_id=user_id)
        
        for i in range(2):
            event_service.create_event(
                user_id=user_id,
                session_id=session["session_id"],
                event_type="user_query",
                content=f"Message {i}"
            )
        
        result = event_service.list_events(user_id)
        assert result["total"] == 2
    
    def test_get_causal_chain(self, event_service, session_service):
        """Test getting causal chain."""
        user_id = str(uuid4())
        session = session_service.create_session(user_id=user_id)
        
        event1 = event_service.create_event(
            user_id=user_id,
            session_id=session["session_id"],
            event_type="user_query",
            content="First message"
        )
        
        event2 = event_service.create_event(
            user_id=user_id,
            session_id=session["session_id"],
            event_type="llm_response",
            content="Response",
            parent_event_id=event1["event_id"],
            causal_chain_id=event1["causal_chain_id"]
        )
        
        chain = event_service.get_causal_chain(event1["causal_chain_id"], user_id)
        assert len(chain) == 2
        assert chain[0]["event_id"] == event1["event_id"]
        assert chain[1]["event_id"] == event2["event_id"]
    
    def test_delete_event(self, event_service, session_service):
        """Test deleting event."""
        user_id = str(uuid4())
        session = session_service.create_session(user_id=user_id)
        
        event = event_service.create_event(
            user_id=user_id,
            session_id=session["session_id"],
            event_type="user_query",
            content="Message"
        )
        
        event_service.delete_event(event["event_id"], user_id)
        
        with pytest.raises(Exception):
            event_service.get_event(event["event_id"], user_id)
    
    def test_get_event_permission_denied(self, event_service, session_service):
        """Test permission denied when accessing other user's event."""
        user_id = str(uuid4())
        other_user_id = str(uuid4())
        session = session_service.create_session(user_id=user_id)
        
        event = event_service.create_event(
            user_id=user_id,
            session_id=session["session_id"],
            event_type="user_query",
            content="Message"
        )
        
        with pytest.raises(Exception):
            event_service.get_event(event["event_id"], other_user_id)
    
    def test_create_event_with_metadata(self, event_service, session_service):
        """Test creating event with metadata."""
        user_id = str(uuid4())
        session = session_service.create_session(user_id=user_id)
        
        metadata = {"key": "value", "nested": {"data": 123}}
        event = event_service.create_event(
            user_id=user_id,
            session_id=session["session_id"],
            event_type="user_query",
            content="Message",
            metadata=metadata
        )
        
        assert event["metadata"] == metadata
        
        retrieved = event_service.get_event(event["event_id"], user_id)
        assert retrieved["metadata"] == metadata
