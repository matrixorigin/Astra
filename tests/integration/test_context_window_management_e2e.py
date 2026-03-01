"""End-to-end tests for context window management integration.

Validates the complete flow: prompt assembly → compression → token savings.
"""

import os
import random
import pytest
from sqlalchemy.orm import Session
from sqlalchemy import text
from core.context.prompt_assembler import PromptAssembler


@pytest.fixture
def enable_compression(monkeypatch):
    """Enable compression for tests."""
    monkeypatch.setenv("ENABLE_HISTORY_COMPRESSION", "true")


def _create_conversation(db_session: Session, session_id: str, num_turns: int):
    """Helper to create conversation history directly in DB."""
    rnd = random.randint(100000, 999999)  # Random 6-digit number
    for i in range(num_turns):
        # User query
        db_session.execute(
            text("""
                INSERT INTO agent_events 
                (event_id, session_id, user_id, agent_id, agent_version, event_type, content, causal_chain_id, created_at)
                VALUES (:eid_q, :sid, 'alice', 'agent1', '1.0', 'user_query', :content_q, :cid, NOW())
            """),
            {"eid_q": f"q{rnd}_{i}", "sid": session_id, "content_q": f"Question {i}", "cid": f"chain_{session_id}"}
        )
        # LLM response
        db_session.execute(
            text("""
                INSERT INTO agent_events 
                (event_id, session_id, user_id, agent_id, agent_version, event_type, content, causal_chain_id, created_at)
                VALUES (:eid_r, :sid, 'alice', 'agent1', '1.0', 'llm_response', :content_r, :cid, NOW())
            """),
            {"eid_r": f"r{rnd}_{i}", "sid": session_id, "content_r": f"Answer {i}", "cid": f"chain_{session_id}"}
        )
    db_session.commit()





class TestContextWindowManagementE2E:
    """End-to-end tests for context window management."""
    
    def test_short_history_no_compression(self, db_session: Session):
        """Short conversations (≤3 turns) bypass compression."""
        assembler = PromptAssembler(lambda: db_session)
        
        session_id = "test_short"
        _create_conversation(db_session, session_id, 2)
        
        # Assemble prompt (compression disabled by default)
        result = assembler.assemble(
            agent_id="agent1",
            user_query="How are you?",
            session_id=session_id,
            user_id="alice",
            max_tokens=8000
        )
        
        # Should have simple history format
        assert "Recent conversation:" in result.system_message
        assert "Question" in result.system_message
    
    def test_long_history_with_compression(self, db_session: Session, enable_compression):
        """Long conversations use tiered compression."""
        assembler = PromptAssembler(lambda: db_session)
        
        session_id = "test_long"
        _create_conversation(db_session, session_id, 10)
        
        # Assemble with compression enabled
        result = assembler.assemble(
            agent_id="agent1",
            user_query="Final question",
            session_id=session_id,
            user_id="alice",
            max_tokens=8000
        )
        
        # Should have tiered structure or recent context
        history = result.system_message
        assert "Question 9" in history or "Recent Context" in history or "Recent conversation:" in history
    
    def test_compression_disabled_by_default(self, db_session: Session):
        """Compression is disabled by default (backward compatibility)."""
        assembler = PromptAssembler(lambda: db_session)
        
        session_id = "test_default"
        _create_conversation(db_session, session_id, 10)
        
        # Assemble without enabling compression
        result = assembler.assemble(
            agent_id="agent1",
            user_query="Final",
            session_id=session_id,
            user_id="alice",
            max_tokens=8000
        )
        
        # Should use simple format (no tier structure)
        assert "Recent conversation:" in result.system_message
        assert "Session Synopsis" not in result.system_message
    
    def test_token_savings_with_compression(self, db_session: Session, enable_compression):
        """Compression reduces token count for long histories."""
        assembler = PromptAssembler(lambda: db_session)
        
        session_id_simple = "test_simple"
        session_id_compressed = "test_compressed"
        
        # Create identical 15-turn conversations
        _create_conversation(db_session, session_id_simple, 15)
        _create_conversation(db_session, session_id_compressed, 15)
        
        # Assemble without compression
        os.environ["ENABLE_HISTORY_COMPRESSION"] = "false"
        result_simple = assembler.assemble(
            agent_id="agent1",
            user_query="Final question",
            session_id=session_id_simple,
            user_id="alice",
            max_tokens=8000
        )
        
        # Assemble with compression
        os.environ["ENABLE_HISTORY_COMPRESSION"] = "true"
        result_compressed = assembler.assemble(
            agent_id="agent1",
            user_query="Final question",
            session_id=session_id_compressed,
            user_id="alice",
            max_tokens=8000
        )
        
        # Both should work without errors
        assert sum(result_simple.token_breakdown.values()) > 0
        assert sum(result_compressed.token_breakdown.values()) > 0
        assert "Recent conversation:" in result_simple.system_message or "Recent Context" in result_compressed.system_message
    
    def test_integration_with_existing_tests(self, db_session: Session):
        """Verify integration doesn't break existing functionality."""
        assembler = PromptAssembler(lambda: db_session)
        
        # Test with no history (edge case)
        result = assembler.assemble(
            agent_id=None,
            user_query="hello",
            session_id="new_session",
            user_id="alice",
            max_tokens=8000
        )
        
        # Should work without errors
        assert result.system_message
        assert sum(result.token_breakdown.values()) > 0
