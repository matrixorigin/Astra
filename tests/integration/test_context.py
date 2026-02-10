"""Integration tests for Context Management (Phase 3)."""

import pytest
from core.context import ContextManager, TaskType
from core.events.event_logger import EventLogger
from core.events.session_manager import SessionManager
from sdk import Database


@pytest.fixture
def db():
    """Database connection."""
    return Database()


@pytest.fixture
def context_manager(db):
    """Context manager instance with snapshots enabled."""
    return ContextManager(db, enable_snapshots=True)


@pytest.fixture
def session_with_events(db):
    """Create a session with sample events."""
    session_mgr = SessionManager(db)
    logger = EventLogger(db)
    
    # Create session with unique user_id to avoid conflicts
    import time
    user_id = f"test_user_{int(time.time() * 1000)}"
    session = session_mgr.create_session(user_id=user_id)
    
    # Add events
    events = []
    for i in range(10):
        event = logger.create_user_query(
            user_id=user_id,
            session_id=session.session_id,
            content=f"Query {i}: How to implement authentication?"
        )
        events.append(event)
        
        response = logger.create_llm_response(
            user_id=user_id,
            session_id=session.session_id,
            content=f"Response {i}: Use JWT tokens for authentication.",
            agent_id="test-agent",
            agent_version="1.0.0",
            parent_event_id=event.event_id,
            causal_chain_id=event.causal_chain_id
        )
        events.append(response)
    
    yield session, events
    
    # Cleanup after test
    db.execute("DELETE FROM conversation_events WHERE session_id = %s", (session.session_id,))
    db.execute("DELETE FROM sessions WHERE session_id = %s", (session.session_id,))
    db.execute("DELETE FROM context_snapshots WHERE session_id = %s", (session.session_id,))


def test_context_manager_build_context(context_manager, session_with_events):
    """Test building context from session events."""
    session, events = session_with_events
    
    # Build context
    context = context_manager.build_context(
        session_id=session.session_id,
        query="How do I implement JWT authentication?",
        max_tokens=4000,
        task_type=TaskType.GENERAL
    )
    
    # Verify context structure
    assert context.system_prompt is not None
    assert isinstance(context.selected_events, list)
    assert len(context.selected_events) > 0
    assert context.total_tokens > 0
    assert context.assembly_time_ms > 0
    assert context.task_type == TaskType.GENERAL
    
    # Verify token budget
    assert 'system' in context.token_budget
    assert 'history' in context.token_budget
    assert context.total_tokens <= 4000


def test_context_manager_task_aware_allocation(context_manager, session_with_events):
    """Test task-aware token allocation."""
    session, _ = session_with_events
    
    # Code review task
    code_context = context_manager.build_context(
        session_id=session.session_id,
        query="Review this PR",
        max_tokens=8000,
        task_type=TaskType.CODE_REVIEW
    )
    
    # Planning task
    planning_context = context_manager.build_context(
        session_id=session.session_id,
        query="Plan the architecture",
        max_tokens=8000,
        task_type=TaskType.PLANNING
    )
    
    # Code review should allocate more to code
    assert code_context.token_budget['code'] > planning_context.token_budget['code']
    
    # Planning should allocate more to history
    assert planning_context.token_budget['history'] > code_context.token_budget['history']


def test_context_manager_relevance_scoring(context_manager, session_with_events):
    """Test relevance scoring prioritizes relevant events."""
    session, events = session_with_events
    
    # Add a highly relevant event
    logger = EventLogger(context_manager.db)
    user_id = session.user_id  # Use same user_id from session
    relevant_event = logger.create_user_query(
        user_id=user_id,
        session_id=session.session_id,
        content="JWT authentication implementation details"
    )
    
    # Build context with JWT query
    context = context_manager.build_context(
        session_id=session.session_id,
        query="How to implement JWT authentication?",
        max_tokens=4000
    )
    
    # Verify relevant event is included
    event_ids = [e['event_id'] for e in context.selected_events]
    assert relevant_event.event_id in event_ids
    
    # Verify it has high score (semantic + temporal + keyword)
    score = context.relevance_scores.get(relevant_event.event_id, 0)
    assert score > 0.15  # Should have reasonable score from semantic similarity


def test_context_snapshot_save_and_load(context_manager, session_with_events):
    """Test saving and loading context snapshots."""
    session, events = session_with_events
    
    # Build context
    context = context_manager.build_context(
        session_id=session.session_id,
        query="Test query",
        max_tokens=4000
    )
    
    # Save snapshot (without event_id to avoid UUID issue)
    snapshot_id = context_manager.save_snapshot(
        context=context,
        session_id=session.session_id
    )
    
    assert snapshot_id is not None
    
    # Load snapshot
    loaded_context = context_manager.load_snapshot(snapshot_id)
    
    # Verify loaded context matches original
    assert loaded_context.system_prompt == context.system_prompt
    assert loaded_context.total_tokens == context.total_tokens
    assert loaded_context.task_type == context.task_type
    assert len(loaded_context.selected_events) == len(context.selected_events)


def test_context_to_prompt(context_manager, session_with_events):
    """Test converting context to LLM prompt."""
    session, _ = session_with_events
    
    context = context_manager.build_context(
        session_id=session.session_id,
        query="Test query",
        max_tokens=4000
    )
    
    prompt = context.to_prompt()
    
    # Verify prompt structure
    assert isinstance(prompt, str)
    assert len(prompt) > 0
    assert context.system_prompt in prompt
    assert "Conversation History" in prompt


def test_context_manager_empty_session(context_manager, db):
    """Test building context for empty session."""
    import time
    user_id = f"test_user_{int(time.time() * 1000)}"
    
    session_mgr = SessionManager(db)
    session = session_mgr.create_session(user_id=user_id)
    
    # Build context for empty session
    context = context_manager.build_context(
        session_id=session.session_id,
        query="First query",
        max_tokens=4000
    )
    
    # Should still return valid context
    assert context is not None
    assert len(context.selected_events) == 0
    assert context.total_tokens > 0  # System prompt still counts
    
    # Cleanup
    db.execute("DELETE FROM sessions WHERE session_id = %s", (session.session_id,))


def test_context_manager_token_budget_respected(context_manager, session_with_events):
    """Test that token budget is respected."""
    session, _ = session_with_events
    
    # Build with small budget
    context = context_manager.build_context(
        session_id=session.session_id,
        query="Test query",
        max_tokens=2000
    )
    
    # Total tokens should not exceed budget
    assert context.total_tokens <= 2000
    
    # Build with large budget
    large_context = context_manager.build_context(
        session_id=session.session_id,
        query="Test query",
        max_tokens=8000
    )
    
    # Should include more events
    assert len(large_context.selected_events) >= len(context.selected_events)
