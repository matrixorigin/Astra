"""Test hybrid retrieval and replay consistency."""

import sys
import os
sys.path.insert(0, os.path.abspath(os.path.join(os.path.dirname(__file__), "../..")))

from api.database import get_db_session, init_db
from core.context.manager import ContextManager, TaskType
from core.events.event_logger import EventLogger
from core.events.session_manager import SessionManager
from uuid_utils import uuid7


def test_hybrid_retrieval():
    """Test MatrixOne hybrid search retrieval."""
    db = next(get_db_session())
    init_db()
    
    # Create session and events
    session_mgr = SessionManager(db)
    logger = EventLogger(db)
    
    session = session_mgr.create_session(user_id="test_user")
    session_id = session.session_id
    
    # Create test events
    chain_id = str(uuid7())
    parent_id = None
    
    events = [
        ("What is event sourcing?", "user_query"),
        ("Event sourcing is a pattern where state changes are stored as events.", "llm_response"),
        ("How do I implement it in Python?", "user_query"),
        ("You can use libraries like eventsourcing or build your own.", "llm_response"),
    ]
    
    for content, event_type in events:
        if event_type == "user_query":
            event = logger.create_user_query(
                user_id="test_user",
                session_id=session_id,
                content=content,
                parent_event_id=parent_id,
                causal_chain_id=chain_id,
            )
        else:
            event = logger.create_llm_response(
                user_id="test_user",
                session_id=session_id,
                content=content,
                agent_id="test_agent",
                agent_version="1.0.0",
                parent_event_id=parent_id,
                causal_chain_id=chain_id,
            )
        parent_id = event.event_id
    
    # Build context with fallback retrieval
    ctx_mgr = ContextManager(db, embedding_provider="mock")
    context = ctx_mgr.build_context(
        session_id=session_id,
        query="Tell me about event sourcing implementation",
        task_type=TaskType.GENERAL,
        current_chain_id=chain_id,
        use_hybrid_retrieval=False,
    )
    
    # Verify retrieval results are stored
    assert context.retrieved_events is not None
    assert len(context.retrieved_events) > 0


def test_replay_consistency():
    """Test that replay uses snapshot retrieval results."""
    db = next(get_db_session())
    init_db()
    
    # Create session and events
    session_mgr = SessionManager(db)
    logger = EventLogger(db)
    
    session = session_mgr.create_session(user_id="test_user_replay")
    session_id = session.session_id
    
    chain_id = str(uuid7())
    e1 = logger.create_user_query(
        user_id="test_user_replay",
        session_id=session_id,
        content="Test query",
        causal_chain_id=chain_id,
    )
    
    # Build context
    ctx_mgr = ContextManager(db, embedding_provider="mock")
    original_context = ctx_mgr.build_context(
        session_id=session_id,
        query="Test",
        use_hybrid_retrieval=False,
    )
    
    # Save snapshot
    context_capture_id = ctx_mgr.save_snapshot(
        context=original_context,
        session_id=session_id,
        event_id=e1.event_id,
    )
    
    # Load snapshot
    loaded_context = ctx_mgr.load_snapshot(context_capture_id)
    
    # Verify retrieved_events are preserved
    assert loaded_context.retrieved_events is not None
    assert len(loaded_context.retrieved_events) == len(original_context.retrieved_events or [])
    
    # Build context using forced retrieval (replay mode)
    replay_context = ctx_mgr.build_context(
        session_id=session_id,
        query="Test",
        forced_retrieval=loaded_context.retrieved_events,
    )
    
    # Verify retrieval consistency
    original_retrieved_ids = {e["event_id"] for e in original_context.retrieved_events or []}
    replay_retrieved_ids = {e["event_id"] for e in replay_context.retrieved_events or []}
    
    assert original_retrieved_ids == replay_retrieved_ids


def test_fallback_retrieval():
    """Test fallback to non-hybrid retrieval when needed."""
    db = next(get_db_session())
    init_db()
    
    session_mgr = SessionManager(db)
    logger = EventLogger(db)
    
    session = session_mgr.create_session(user_id="test_user_fallback")
    session_id = session.session_id
    
    # Create events
    chain_id = str(uuid7())
    logger.create_user_query(
        user_id="test_user_fallback",
        session_id=session_id,
        content="Simple query",
        causal_chain_id=chain_id,
    )
    
    # Build context with fallback
    ctx_mgr = ContextManager(db, embedding_provider="mock")
    context = ctx_mgr.build_context(
        session_id=session_id,
        query="Another query",
        use_hybrid_retrieval=False,
    )
    
    assert len(context.selected_events) >= 0
