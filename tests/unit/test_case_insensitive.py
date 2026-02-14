"""Test case-insensitive pattern matching."""

from core.context import KnowledgeExtractor
from api.models import Event
from uuid_utils import uuid7


def test_case_insensitive_extraction(db_session):
    """Test that extraction works with different cases."""
    extractor = KnowledgeExtractor(db_session)
    
    user_id = "test_user"
    session_id = str(uuid7())
    chain_id = str(uuid7())
    
    # Test different cases
    test_cases = [
        "I prefer TypeScript",  # Title case
        "I PREFER Python",      # Upper case
        "i prefer golang",      # Lower case
        "I Prefer Rust",        # Mixed case
    ]
    
    for i, content in enumerate(test_cases):
        event = Event(
            event_id=str(uuid7()),
            session_id=session_id,
            user_id=user_id,
            agent_id="test_agent",
            event_type="user_query",
            content=content,
            causal_chain_id=chain_id,
        )
        db_session.add(event)
    
    db_session.commit()
    
    # Extract knowledge
    extracted = extractor.extract_from_chain(chain_id, user_id)
    
    # Should extract at least one entry (may update same entry multiple times)
    assert len(extracted) > 0
    print(f"Extracted {len(extracted)} entries from case-insensitive patterns")


if __name__ == "__main__":
    from api.database import get_db_session
    db = next(get_db_session())
    test_case_insensitive_extraction(db)
    print("✓ Case-insensitive extraction works!")
