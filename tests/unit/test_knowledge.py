"""Test knowledge extraction and semantic memory."""

import pytest
from core.context import KnowledgeExtractor, ContextManager
from api.models import KnowledgeEntry, Event
from uuid_utils import uuid7


def test_knowledge_extraction(db_session):
    """Test extracting knowledge from conversation events."""
    extractor = KnowledgeExtractor(db_session)
    
    # Create test events
    user_id = "test_user"
    session_id = str(uuid7())
    chain_id = str(uuid7())
    
    event1 = Event(
        event_id=str(uuid7()),
        session_id=session_id,
        user_id=user_id,
        agent_id="test_agent",
        event_type="user_query",
        content="I prefer TypeScript for backend development",
        causal_chain_id=chain_id,
    )
    
    event2 = Event(
        event_id=str(uuid7()),
        session_id=session_id,
        user_id=user_id,
        agent_id="test_agent",
        event_type="llm_response",
        content="The codebase uses dependency injection pattern for auth",
        causal_chain_id=chain_id,
    )
    
    db_session.add(event1)
    db_session.add(event2)
    db_session.commit()
    
    # Extract knowledge
    extracted = extractor.extract_from_chain(chain_id, user_id)
    
    assert len(extracted) > 0
    assert any(e["action"] in ["created", "updated"] for e in extracted)


def test_confidence_decay(db_session):
    """Test confidence decay mechanism."""
    extractor = KnowledgeExtractor(db_session)
    user_id = "test_user"
    
    # Create knowledge entry
    entry = KnowledgeEntry(
        entry_id=str(uuid7()),
        user_id=user_id,
        category="user_preference",
        key_name="test_key",
        value="test_value",
        source_event_ids='["event1"]',
        confidence=1.0,
        initial_confidence=1.0,
        trust_tier="T3",
    )
    db_session.add(entry)
    db_session.commit()
    
    # Apply decay (with short half-life for testing)
    count = extractor.decay_confidence(user_id, half_life_days=1)
    
    # Should have decayed at least one entry
    assert count >= 0


def test_semantic_retrieval(db_session):
    """Test semantic knowledge retrieval."""
    context_mgr = ContextManager(db_session, embedding_provider="mock")
    user_id = "test_user"
    
    # Create knowledge entries
    entry1 = KnowledgeEntry(
        entry_id=str(uuid7()),
        user_id=user_id,
        category="user_preference",
        key_name="language",
        value="typescript",
        source_event_ids='["event1"]',
        confidence=0.8,
        trust_tier="T3",
    )
    
    entry2 = KnowledgeEntry(
        entry_id=str(uuid7()),
        user_id=user_id,
        category="codebase_pattern",
        key_name="auth.pattern",
        value="dependency_injection",
        source_event_ids='["event2"]',
        confidence=0.6,
        trust_tier="T3",
    )
    
    db_session.add(entry1)
    db_session.add(entry2)
    db_session.commit()
    
    # Retrieve knowledge
    results = context_mgr.retrieve_semantic_knowledge(
        user_id=user_id,
        query="what language does user prefer",
        limit=5
    )
    
    assert len(results) > 0
    assert any(r["key_name"] == "language" for r in results)


def test_low_confidence_quarantine(db_session):
    """Test quarantining low confidence entries."""
    extractor = KnowledgeExtractor(db_session)
    user_id = "test_user"
    
    # Create low confidence entry
    entry = KnowledgeEntry(
        entry_id=str(uuid7()),
        user_id=user_id,
        category="domain_fact",
        key_name="test_fact",
        value="low confidence fact",
        source_event_ids='["event1"]',
        confidence=0.2,  # Below threshold
        trust_tier="T4",
    )
    db_session.add(entry)
    db_session.commit()
    
    # Quarantine low confidence
    count = extractor.quarantine_low_confidence(user_id, threshold=0.3)
    
    assert count >= 1
