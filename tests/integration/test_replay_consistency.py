"""Minimal test for hybrid retrieval and replay consistency."""

import sys
import os
sys.path.insert(0, os.path.abspath(os.path.join(os.path.dirname(__file__), "../..")))

from api.database import get_db_session, init_db
from core.context.manager import ContextManager, TaskType, Context
from core.events.event_logger import EventLogger
from core.events.session_manager import SessionManager
from uuid_utils import uuid7


def test_retrieval_storage_and_replay():
    """Test that retrieval results are stored and can be used for replay."""
    print("\n=== Test: Retrieval Storage & Replay Consistency ===\n")
    
    db = next(get_db_session())
    init_db()
    
    # Create session and events
    session_mgr = SessionManager(db)
    logger = EventLogger.from_session(db)
    
    session = session_mgr.create_session(user_id="test_user")
    session_id = session.session_id
    
    chain_id = str(uuid7())
    
    # Create 3 events
    e1 = logger.create_user_query(
        user_id="test_user",
        session_id=session_id,
        content="What is event sourcing?",
        causal_chain_id=chain_id,
    )
    
    e2 = logger.create_llm_response(
        user_id="test_user",
        session_id=session_id,
        content="Event sourcing is a pattern.",
        agent_id="test_agent",
        agent_version="1.0.0",
        parent_event_id=e1.event_id,
        causal_chain_id=chain_id,
    )
    
    e3 = logger.create_user_query(
        user_id="test_user",
        session_id=session_id,
        content="How to implement it?",
        parent_event_id=e2.event_id,
        causal_chain_id=chain_id,
    )
    
    print(f"✓ Created 3 events in session {session_id}")
    
    # Build context (will retrieve and store results)
    ctx_mgr = ContextManager(lambda: db, embedding_provider="mock")
    
    # Manually create retrieved_events (simulating retrieval)
    retrieved_events = [
        {
            "event_id": e1.event_id,
            "event_type": "user_query",
            "content": e1.content,
            "relevance_score": 0.9,
        },
        {
            "event_id": e2.event_id,
            "event_type": "llm_response",
            "content": e2.content,
            "relevance_score": 0.8,
        },
        {
            "event_id": e3.event_id,
            "event_type": "user_query",
            "content": e3.content,
            "relevance_score": 0.7,
        },
    ]
    
    # Create context manually
    context = Context(
        system_prompt="Test prompt",
        skill_definitions=[],
        selected_events=[
            {"event_id": e1.event_id, "event_type": "user_query", "content": e1.content, "score": 0.9},
            {"event_id": e2.event_id, "event_type": "llm_response", "content": e2.content, "score": 0.8},
        ],
        code_context=[],
        documentation=[],
        total_tokens=100,
        token_budget={"system": 500, "skills": 0, "history": 100, "code": 0, "docs": 0, "reserve": 0},
        assembly_time_ms=10,
        relevance_scores={e1.event_id: 0.9, e2.event_id: 0.8},
        task_type=TaskType.GENERAL,
        retrieved_events=retrieved_events,  # Store retrieval results
    )
    
    print(f"✓ Created context with {len(retrieved_events)} retrieved events")
    
    # Save snapshot
    context_capture_id = ctx_mgr.save_snapshot(
        context=context,
        session_id=session_id,
        event_id=e3.event_id,
    )
    
    print(f"✓ Saved snapshot: {context_capture_id}")
    
    # Load snapshot
    ctx_mgr.flush_writes()
    loaded_context = ctx_mgr.load_snapshot(context_capture_id)
    
    print(f"✓ Loaded snapshot")
    print(f"  - Retrieved events: {len(loaded_context.retrieved_events or [])}")
    print(f"  - Selected events: {len(loaded_context.selected_events)}")
    
    # Verify retrieved_events are preserved
    assert loaded_context.retrieved_events is not None, "retrieved_events should not be None"
    assert len(loaded_context.retrieved_events) == 3, f"Expected 3 retrieved events, got {len(loaded_context.retrieved_events)}"
    
    # Verify event IDs match
    loaded_ids = {e["event_id"] for e in loaded_context.retrieved_events}
    original_ids = {e["event_id"] for e in retrieved_events}
    assert loaded_ids == original_ids, "Event IDs should match"
    
    print(f"✓ Replay consistency verified: same {len(original_ids)} events")
    
    # Test forced retrieval (replay mode)
    # In replay, we should get the same retrieval results, then re-select based on budget
    replay_context = ctx_mgr.build_context(
        session_id=session_id,
        query="Test query",
        forced_retrieval=loaded_context.retrieved_events,
        max_tokens=8000,  # Same budget
    )
    
    print(f"✓ Built replay context with forced retrieval")
    print(f"  - Retrieved {len(replay_context.retrieved_events or [])} events (forced)")
    print(f"  - Selected {len(replay_context.selected_events)} events")
    
    # Key insight: Replay guarantees same RETRIEVAL, not necessarily same SELECTION
    # Selection depends on token budget and may vary
    # But retrieval should be identical
    assert replay_context.retrieved_events is not None
    assert len(replay_context.retrieved_events) == len(loaded_context.retrieved_events)
    
    replay_retrieved_ids = {e["event_id"] for e in replay_context.retrieved_events}
    loaded_retrieved_ids = {e["event_id"] for e in loaded_context.retrieved_events}
    
    assert replay_retrieved_ids == loaded_retrieved_ids, "Replay retrieval should be identical"
    
    print(f"✓ Replay retrieval consistency verified: same {len(replay_retrieved_ids)} events retrieved")
    print(f"  (Selection may vary based on token budget, but retrieval is deterministic)")
    
    print("\n" + "=" * 60)
    print("✓ All tests passed!")
    print("=" * 60)


if __name__ == "__main__":
    test_retrieval_storage_and_replay()
