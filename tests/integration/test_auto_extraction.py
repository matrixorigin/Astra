"""Test automatic knowledge extraction on session close."""

from core.events import EventLogger, SessionManager
from core.context import ContextManager
from api.database import get_db_session


def test_auto_extraction():
    """Test that knowledge is automatically extracted when session closes."""
    db = next(get_db_session())
    
    session_mgr = SessionManager(db)
    event_logger = EventLogger.from_session(db)
    context_mgr = ContextManager(db, embedding_provider="mock")
    
    user_id = "test_auto_user"
    
    # Create session
    session = session_mgr.create_session(user_id=user_id)
    print(f"✓ Created session: {session.session_id}")
    
    # Log conversation with preference
    user_event = event_logger.create_user_query(
        user_id=user_id,
        session_id=session.session_id,
        content="I PREFER TypeScript for backend development",  # Upper case test
    )
    print(f"✓ Logged user query")
    
    llm_event = event_logger.create_llm_response(
        user_id=user_id,
        session_id=session.session_id,
        content="Got it, I'll remember your preference for TypeScript.",
        agent_id="test-agent",
        agent_version="1.0.0",
        parent_event_id=user_event.event_id,
        causal_chain_id=user_event.causal_chain_id,
    )
    print(f"✓ Logged LLM response")
    
    # Close session - should auto-extract knowledge
    session_mgr.close_session(session.session_id)
    print(f"✓ Session closed (auto-extraction triggered)")
    
    # Verify knowledge was extracted
    results = context_mgr.retrieve_semantic_knowledge(
        user_id=user_id,
        query="user preferences",
        limit=5
    )
    
    print(f"\n--- Retrieved Knowledge ---")
    for r in results:
        print(f"  • {r['category']}.{r['key_name']}: {r['value']}")
        print(f"    confidence={r['confidence']:.2f}, relevance={r['relevance']:.2f}")
    
    assert len(results) > 0, "No knowledge extracted!"
    print(f"\n✓ Auto-extraction works! Found {len(results)} entries")


if __name__ == "__main__":
    test_auto_extraction()
