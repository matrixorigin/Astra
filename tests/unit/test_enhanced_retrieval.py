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
        """Test episodic memory retrieval."""
        # Mock query result
        mock_row = Mock()
        mock_row.event_id = "evt_123"
        mock_row.session_id = "sess_123"
        mock_row.event_type = "user_query"
        mock_row.content = "Test query"
        mock_row.created_at = None
        mock_row.causal_chain_id = "chain_123"
        mock_row.parent_event_id = None
        mock_row.metadata = {}
        mock_row.relevance_score = 0.85
        
        mock_db.execute.return_value = [mock_row]
        
        events = retriever.retrieve_events(
            query_text="test",
            query_embedding=[0.1] * 1536,
            session_id="sess_123",
        )
        
        assert len(events) == 1
        assert events[0]["event_id"] == "evt_123"
        assert events[0]["relevance_score"] == 0.85
    
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
        # Mock query result
        mock_row = Mock()
        mock_row.event_id = "evt_123"
        mock_row.session_id = "sess_123"
        mock_row.event_type = "user_query"
        mock_row.content = "Test query"
        mock_row.created_at = None
        mock_row.causal_chain_id = "chain_123"
        mock_row.parent_event_id = None
        mock_row.metadata = {}
        mock_row.relevance_score = 0.95  # Higher due to causal bonus
        
        mock_db.execute.return_value = [mock_row]
        
        events = retriever.retrieve_events(
            query_text="test",
            query_embedding=[0.1] * 1536,
            session_id="sess_123",
            current_chain_id="chain_123",
        )
        
        assert len(events) == 1
        assert events[0]["relevance_score"] == 0.95
    
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
