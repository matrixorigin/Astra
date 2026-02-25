"""Example: Using semantic memory for knowledge extraction and retrieval.

Demonstrates:
1. Extracting knowledge from conversation events
2. Retrieving relevant knowledge by query
3. Applying confidence decay
"""

from core.context import KnowledgeExtractor, ContextManager
from core.events import EventLogger, SessionManager
from api.database import get_db_session, SessionLocal


def main():
    """Run semantic memory example."""
    db = next(get_db_session())
    
    # Initialize components
    session_mgr = SessionManager(db)
    event_logger = EventLogger.from_session(db)
    extractor = KnowledgeExtractor(db)
    context_mgr = ContextManager(SessionLocal, embedding_provider="mock")
    
    user_id = "alice"
    
    # 1. Create a conversation session
    session = session_mgr.create_session(user_id=user_id)
    print(f"✓ Created session: {session.session_id}")
    
    # 2. Log conversation events
    user_event = event_logger.create_user_query(
        user_id=user_id,
        session_id=session.session_id,
        content="I prefer TypeScript for backend development. Our codebase uses dependency injection pattern.",
    )
    print(f"✓ Logged user query: {user_event.event_id}")
    
    llm_event = event_logger.create_llm_response(
        user_id=user_id,
        session_id=session.session_id,
        content="I understand you prefer TypeScript. I'll keep that in mind for code suggestions.",
        agent_id="dev-agent",
        agent_version="1.0.0",
        parent_event_id=user_event.event_id,
        causal_chain_id=user_event.causal_chain_id,
    )
    print(f"✓ Logged LLM response: {llm_event.event_id}")
    
    # 3. Extract knowledge from the conversation
    print("\n--- Extracting Knowledge ---")
    extracted = extractor.extract_from_chain(
        causal_chain_id=user_event.causal_chain_id,
        user_id=user_id
    )
    
    for entry in extracted:
        print(f"  • {entry['action'].upper()}: entry_id={entry['entry_id']}, confidence={entry['confidence']}")
    
    # 4. Retrieve knowledge by query
    print("\n--- Retrieving Knowledge ---")
    queries = [
        "what language does user prefer",
        "what patterns are used in codebase",
        "user preferences"
    ]
    
    for query in queries:
        results = context_mgr.retrieve_semantic_knowledge(
            user_id=user_id,
            query=query,
            limit=3
        )
        
        print(f"\nQuery: '{query}'")
        if results:
            for r in results:
                print(f"  • {r['category']}.{r['key_name']}: {r['value']}")
                print(f"    confidence={r['confidence']:.2f}, relevance={r['relevance']:.2f}")
        else:
            print("  (no results)")
    
    # 5. Apply confidence decay
    print("\n--- Applying Confidence Decay ---")
    decayed_count = extractor.decay_confidence(user_id=user_id, half_life_days=60)
    print(f"✓ Applied decay to {decayed_count} entries")
    
    # 6. Check for low confidence entries
    print("\n--- Checking Low Confidence Entries ---")
    quarantine_count = extractor.quarantine_low_confidence(user_id=user_id, threshold=0.3)
    print(f"✓ Found {quarantine_count} entries below confidence threshold")
    
    # 7. Close session
    session_mgr.close_session(session.session_id)
    print(f"\n✓ Session closed: {session.session_id}")


if __name__ == "__main__":
    main()
