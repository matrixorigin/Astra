"""Integration tests for session and causal chain management.

Tests complete conversation flows with sessions and event chains.
"""

import pytest
from ulid import ULID

from core.events.causal_chain import CausalChainManager
from core.events.event_logger import EventLogger
from core.events.event_reader import EventReader
from core.events.models import EventType
from core.events.session_manager import SessionManager
from core.events.session_models import SessionStatus
from sdk.database import Database


@pytest.fixture
def db():
    """Database fixture."""
    return Database()


@pytest.fixture
def session_manager(db):
    """Session manager fixture."""
    return SessionManager(db)


@pytest.fixture
def event_logger(db):
    """Event logger fixture."""
    return EventLogger(db)


@pytest.fixture
def event_reader(db):
    """Event reader fixture."""
    return EventReader(db)


@pytest.fixture
def causal_chain_manager(db):
    """Causal chain manager fixture."""
    return CausalChainManager(db)


def test_complete_conversation_flow(
    session_manager, event_logger, event_reader, causal_chain_manager
):
    """Test a complete conversation flow with session and events."""
    user_id = f"test_user_{ULID()}"

    # 1. Create session
    session = session_manager.create_session(user_id=user_id)
    assert session.session_id is not None
    assert session.status == SessionStatus.ACTIVE
    assert session.event_count == 0

    # 2. User asks a question
    user_event = event_logger.create_user_query(
        user_id=user_id,
        session_id=session.session_id,
        content="How do I implement event logging?",
    )
    causal_chain_id = user_event.causal_chain_id

    # Update session
    session_manager.update_session_activity(session.session_id, user_event.event_id)

    # 3. LLM responds
    llm_event = event_logger.create_llm_response(
        user_id=user_id,
        session_id=session.session_id,
        content="Here's how to implement event logging...",
        agent_id="dev-agent",
        agent_version="0.1.0",
        parent_event_id=user_event.event_id,
        causal_chain_id=causal_chain_id,
        llm_model_used="gpt-4",
        token_usage={"prompt": 100, "completion": 200, "total": 300},
    )

    session_manager.update_session_activity(session.session_id, llm_event.event_id)

    # 4. Verify session state
    updated_session = session_manager.get_session(session.session_id)
    assert updated_session is not None
    assert updated_session.event_count == 2
    assert updated_session.last_event_id == llm_event.event_id

    # 5. Verify causal chain
    chain = causal_chain_manager.get_chain(causal_chain_id)
    assert len(chain) == 2
    assert chain[0].event_id == user_event.event_id
    assert chain[1].event_id == llm_event.event_id
    assert chain[1].parent_event_id == chain[0].event_id

    # 6. Verify chain integrity
    validation = causal_chain_manager.validate_chain_integrity(causal_chain_id)
    assert validation["is_valid"] is True
    assert len(validation["issues"]) == 0

    # 7. Get chain summary
    summary = causal_chain_manager.get_chain_summary(causal_chain_id)
    assert summary["total_events"] == 2
    assert summary["total_tokens"] == 300
    assert EventType.USER_QUERY in summary["event_types"]
    assert EventType.LLM_RESPONSE in summary["event_types"]

    # 8. Close session
    session_manager.close_session(session.session_id)
    closed_session = session_manager.get_session(session.session_id)
    assert closed_session.status == SessionStatus.CLOSED


def test_multi_turn_conversation(session_manager, event_logger, causal_chain_manager):
    """Test a multi-turn conversation."""
    user_id = f"test_user_{ULID()}"
    session = session_manager.create_session(user_id=user_id)

    # Turn 1
    user_event_1 = event_logger.create_user_query(
        user_id=user_id,
        session_id=session.session_id,
        content="What is event sourcing?",
    )
    chain_id = user_event_1.causal_chain_id

    llm_event_1 = event_logger.create_llm_response(
        user_id=user_id,
        session_id=session.session_id,
        content="Event sourcing is...",
        agent_id="dev-agent",
        agent_version="0.1.0",
        parent_event_id=user_event_1.event_id,
        causal_chain_id=chain_id,
    )

    # Turn 2 - new causal chain
    user_event_2 = event_logger.create_user_query(
        user_id=user_id,
        session_id=session.session_id,
        content="Can you give an example?",
    )
    chain_id_2 = user_event_2.causal_chain_id

    llm_event_2 = event_logger.create_llm_response(
        user_id=user_id,
        session_id=session.session_id,
        content="Here's an example...",
        agent_id="dev-agent",
        agent_version="0.1.0",
        parent_event_id=user_event_2.event_id,
        causal_chain_id=chain_id_2,
    )

    # Update session for all events
    for event in [user_event_1, llm_event_1, user_event_2, llm_event_2]:
        session_manager.update_session_activity(session.session_id, event.event_id)

    # Verify session has all events
    updated_session = session_manager.get_session(session.session_id)
    assert updated_session.event_count == 4

    # Verify two separate causal chains
    chain_1 = causal_chain_manager.get_chain(chain_id)
    chain_2 = causal_chain_manager.get_chain(chain_id_2)
    assert len(chain_1) == 2
    assert len(chain_2) == 2


def test_get_user_sessions(session_manager):
    """Test retrieving user sessions."""
    user_id = f"test_user_{ULID()}"

    # Create multiple sessions
    session_1 = session_manager.create_session(user_id=user_id)
    session_2 = session_manager.create_session(user_id=user_id)
    session_manager.close_session(session_1.session_id)

    # Get all sessions
    all_sessions = session_manager.get_user_sessions(user_id)
    assert len(all_sessions) >= 2

    # Get only active sessions
    active_sessions = session_manager.get_user_sessions(
        user_id, status=SessionStatus.ACTIVE
    )
    assert len(active_sessions) >= 1
    assert all(s.status == SessionStatus.ACTIVE for s in active_sessions)


def test_parent_child_relationships(event_logger, causal_chain_manager):
    """Test parent-child event relationships."""
    user_id = f"test_user_{ULID()}"
    session_id = f"test_session_{ULID()}"

    # Create parent event
    parent = event_logger.create_user_query(
        user_id=user_id, session_id=session_id, content="Parent query"
    )

    # Create child event
    child = event_logger.create_llm_response(
        user_id=user_id,
        session_id=session_id,
        content="Child response",
        agent_id="dev-agent",
        agent_version="0.1.0",
        parent_event_id=parent.event_id,
        causal_chain_id=parent.causal_chain_id,
    )

    # Verify parent-child relationship
    retrieved_parent = causal_chain_manager.get_parent_event(child.event_id)
    assert retrieved_parent is not None
    assert retrieved_parent.event_id == parent.event_id

    # Verify children
    children = causal_chain_manager.get_child_events(parent.event_id)
    assert len(children) >= 1
    assert any(c.event_id == child.event_id for c in children)
