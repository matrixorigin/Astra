"""Tests for enhanced hybrid retrieval."""

import pytest
from unittest.mock import Mock, patch, MagicMock
from core.context.hybrid_retrieval import HybridRetriever


class TestEnhancedHybridRetrieval:
    """Test enhanced hybrid retrieval with semantic memory."""
    
    @pytest.fixture
    def mock_db(self):
        """Mock database session."""
        return Mock()
    
    @pytest.fixture
    def retriever(self, mock_db):
        """Create hybrid retriever."""
        return HybridRetriever(mock_db)
    
    def test_retrieve_events(self, retriever, mock_db):
        """Test episodic memory retrieval with hybrid search."""
        # Mock vector search result
        mock_vector_row = Mock()
        mock_vector_row.event_id = "evt_123"
        mock_vector_row.session_id = "sess_123"
        mock_vector_row.event_type = "user_query"
        mock_vector_row.content = "Test query"
        mock_vector_row.created_at = None
        mock_vector_row.causal_chain_id = "chain_123"
        mock_vector_row.parent_event_id = None
        mock_vector_row.metadata = {}
        mock_vector_row.vector_score = 0.35  # semantic weight
        
        # Mock fulltext search result (same event)
        mock_fulltext_row = Mock()
        mock_fulltext_row.event_id = "evt_123"
        mock_fulltext_row.session_id = "sess_123"
        mock_fulltext_row.event_type = "user_query"
        mock_fulltext_row.content = "Test query"
        mock_fulltext_row.created_at = None
        mock_fulltext_row.causal_chain_id = "chain_123"
        mock_fulltext_row.parent_event_id = None
        mock_fulltext_row.metadata = {}
        
        # First call: vector search, second call: fulltext search
        mock_db.execute.side_effect = [
            [mock_vector_row],  # Vector search results
            [mock_fulltext_row],  # Fulltext search results
        ]
        
        events = retriever.retrieve_events(
            query_text="test",
            query_embedding=[0.1] * 1536,
            session_id="sess_123",
        )
        
        assert len(events) == 1
        assert events[0]["event_id"] == "evt_123"
        # Score = vector_score (0.35) + keyword_score (0.25) = 0.60
        assert events[0]["relevance_score"] == pytest.approx(0.60, abs=0.01)
    
    def test_retrieve_knowledge(self, retriever, mock_db):
        """Test semantic memory retrieval."""
        # Mock query result
        mock_row = Mock()
        mock_row.entry_id = "ke_123"
        mock_row.category = "user_preference"
        mock_row.key_name = "language"
        mock_row.value = "python"
        mock_row.confidence = 0.8
        mock_row.trust_tier = "T3"
        mock_row.created_at = None
        mock_row.last_validated_at = None
        mock_row.relevance_score = 0.75
        
        mock_db.execute.return_value = [mock_row]
        
        entries = retriever.retrieve_knowledge(
            query_text="programming language",
            query_embedding=[0.1] * 1536,
            user_id="alice",
        )
        
        assert len(entries) == 1
        assert entries[0]["entry_id"] == "ke_123"
        assert entries[0]["category"] == "user_preference"
        assert entries[0]["relevance_score"] == 0.75
    
    def test_retrieve_knowledge_confidence_filter(self, retriever, mock_db):
        """Test knowledge retrieval with confidence filtering."""
        mock_db.execute.return_value = []
        
        entries = retriever.retrieve_knowledge(
            query_text="test",
            query_embedding=[0.1] * 1536,
            user_id="alice",
            confidence_threshold=0.5,
        )
        
        # Should filter out low confidence entries
        assert len(entries) == 0
    
    def test_retrieve_events_with_causal_bonus(self, retriever, mock_db):
        """Test episodic retrieval with causal chain bonus."""
        # Mock vector search result with causal bonus
        mock_vector_row = Mock()
        mock_vector_row.event_id = "evt_123"
        mock_vector_row.session_id = "sess_123"
        mock_vector_row.event_type = "user_query"
        mock_vector_row.content = "Test query"
        mock_vector_row.created_at = None
        mock_vector_row.causal_chain_id = "chain_123"
        mock_vector_row.parent_event_id = None
        mock_vector_row.metadata = {}
        mock_vector_row.vector_score = 0.55  # semantic + causal bonus
        
        # Mock fulltext search result
        mock_fulltext_row = Mock()
        mock_fulltext_row.event_id = "evt_123"
        mock_fulltext_row.session_id = "sess_123"
        mock_fulltext_row.event_type = "user_query"
        mock_fulltext_row.content = "Test query"
        mock_fulltext_row.created_at = None
        mock_fulltext_row.causal_chain_id = "chain_123"
        mock_fulltext_row.parent_event_id = None
        mock_fulltext_row.metadata = {}
        
        mock_db.execute.side_effect = [
            [mock_vector_row],  # Vector search
            [mock_fulltext_row],  # Fulltext search
        ]
        
        events = retriever.retrieve_events(
            query_text="test",
            query_embedding=[0.1] * 1536,
            session_id="sess_123",
            current_chain_id="chain_123",
        )
        
        assert len(events) == 1
        # Score = 0.55 (vector with causal) + 0.25 (keyword) = 0.80
        assert events[0]["relevance_score"] == pytest.approx(0.80, abs=0.01)
    
    def test_retrieve_knowledge_custom_weights(self, retriever, mock_db):
        """Test knowledge retrieval with custom weights."""
        mock_db.execute.return_value = []
        
        entries = retriever.retrieve_knowledge(
            query_text="test",
            query_embedding=[0.1] * 1536,
            user_id="alice",
            weights={
                "semantic": 0.6,
                "keyword": 0.2,
                "confidence": 0.2,
            },
        )
        
        # Should use custom weights
        assert len(entries) == 0
    
    def test_retrieve_events_error_handling(self, retriever, mock_db):
        """Test error handling in event retrieval."""
        # Both SQL calls fail
        mock_db.execute.side_effect = Exception("Database error")
        
        events = retriever.retrieve_events(
            query_text="test",
            query_embedding=[0.1] * 1536,
            session_id="sess_123",
        )
        
        # Should return empty list on error
        assert events == []
    
    def test_retrieve_knowledge_error_handling(self, retriever, mock_db):
        """Test error handling in knowledge retrieval."""
        mock_db.execute.side_effect = Exception("Database error")
        
        entries = retriever.retrieve_knowledge(
            query_text="test",
            query_embedding=[0.1] * 1536,
            user_id="alice",
        )
        
        # Should return empty list on error
        assert entries == []
    
    def test_retrieve_knowledge_invalid_weights(self, retriever, mock_db):
        """Test knowledge retrieval with invalid weights."""
        entries = retriever.retrieve_knowledge(
            query_text="test",
            query_embedding=[0.1] * 1536,
            user_id="alice",
            weights={"semantic": 0.5},  # Missing keyword and confidence
        )
        
        # Should return empty list with invalid weights
        assert entries == []
    
    def test_retrieve_events_fulltext_boolean_mode(self, retriever, mock_db):
        """Test fulltext search with BOOLEAN MODE filtering via session_id."""
        # Mock vector search (no results)
        mock_db.execute.side_effect = [
            [],  # Vector search returns nothing
            [  # Fulltext search with BOOLEAN MODE filtering
                Mock(
                    event_id="evt_456",
                    session_id="sess_123",
                    event_type="tool_call",
                    content="Execute test function",
                    created_at=None,
                    causal_chain_id="chain_456",
                    parent_event_id="evt_123",
                    metadata={"tool": "executor"},
                )
            ],
        ]
        
        events = retriever.retrieve_events(
            query_text="test",
            query_embedding=[0.1] * 1536,
            session_id="sess_123",
        )
        
        # Should find event via fulltext with session_id filtering
        assert len(events) == 1
        assert events[0]["event_id"] == "evt_456"
        # Score = keyword_score (0.25) only
        assert events[0]["relevance_score"] == pytest.approx(0.25, abs=0.01)
    
    def test_retrieve_events_hybrid_merge(self, retriever, mock_db):
        """Test hybrid search merging vector and fulltext results."""
        # Mock vector search result
        mock_vector_row = Mock()
        mock_vector_row.event_id = "evt_123"
        mock_vector_row.session_id = "sess_123"
        mock_vector_row.event_type = "user_query"
        mock_vector_row.content = "Test query"
        mock_vector_row.created_at = None
        mock_vector_row.causal_chain_id = "chain_123"
        mock_vector_row.parent_event_id = None
        mock_vector_row.metadata = {}
        mock_vector_row.vector_score = 0.35
        
        # Mock fulltext search results (same + new event)
        mock_fulltext_row1 = Mock()
        mock_fulltext_row1.event_id = "evt_123"  # Same as vector
        mock_fulltext_row1.session_id = "sess_123"
        mock_fulltext_row1.event_type = "user_query"
        mock_fulltext_row1.content = "Test query"
        mock_fulltext_row1.created_at = None
        mock_fulltext_row1.causal_chain_id = "chain_123"
        mock_fulltext_row1.parent_event_id = None
        mock_fulltext_row1.metadata = {}
        
        mock_fulltext_row2 = Mock()
        mock_fulltext_row2.event_id = "evt_456"  # New from fulltext
        mock_fulltext_row2.session_id = "sess_123"
        mock_fulltext_row2.event_type = "tool_call"
        mock_fulltext_row2.content = "Test execution"
        mock_fulltext_row2.created_at = None
        mock_fulltext_row2.causal_chain_id = "chain_456"
        mock_fulltext_row2.parent_event_id = "evt_123"
        mock_fulltext_row2.metadata = {}
        
        mock_db.execute.side_effect = [
            [mock_vector_row],  # Vector search
            [mock_fulltext_row1, mock_fulltext_row2],  # Fulltext search
        ]
        
        events = retriever.retrieve_events(
            query_text="test",
            query_embedding=[0.1] * 1536,
            session_id="sess_123",
        )
        
        # Should merge both results
        assert len(events) == 2
        
        # evt_123: vector (0.35) + keyword (0.25) = 0.60
        evt_123 = next(e for e in events if e["event_id"] == "evt_123")
        assert evt_123["relevance_score"] == pytest.approx(0.60, abs=0.01)
        
        # evt_456: keyword only (0.25)
        evt_456 = next(e for e in events if e["event_id"] == "evt_456")
        assert evt_456["relevance_score"] == pytest.approx(0.25, abs=0.01)
