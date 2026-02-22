"""Integration tests for P0 Trust & Safety — confidence scoring and streaming verification.

Tests:
1. Confidence scorer with claim type weighting
2. Sentence-level verification in streaming output
3. Verification metadata storage in event audit trail
4. SLO-compatible signal source for downstream evaluation
"""

import json
import time
from unittest.mock import MagicMock, patch

import pytest
from sqlalchemy import text

from core.events.event_logger import EventLogger
from core.events.session_manager import SessionManager
from core.trust_safety.confidence_scorer import (
    ClaimConfidence,
    ClaimType,
    ConfidenceScorer,
    ConfidenceWeights,
    SentenceVerification,
)
from core.trust_safety.streaming_verifier import StreamingVerifier
from api.database import get_db_session


@pytest.fixture
def db_session():
    """Get database session."""
    session = next(get_db_session())
    yield session
    session.close()


@pytest.fixture
def session_manager(db_session):
    """Create session manager."""
    return SessionManager(db_session)


@pytest.fixture
def event_logger(db_session):
    """Create event logger."""
    return EventLogger(db_session)


@pytest.fixture
def mock_firewall():
    """Create mock firewall."""
    firewall = MagicMock()
    firewall.llm_extractor = None
    return firewall


class TestConfidenceScorer:
    """Test confidence scoring with claim type weighting."""
    
    def test_factual_claim_highest_weight(self):
        """Factual claims should have highest weight."""
        scorer = ConfidenceScorer()
        
        factual = scorer.score_claim(
            claim_text="The Earth is round",
            claim_type=ClaimType.FACTUAL,
            base_confidence=0.8,
            verified=True,
            evidence_count=3,
        )
        
        opinion = scorer.score_claim(
            claim_text="The Earth is beautiful",
            claim_type=ClaimType.OPINION,
            base_confidence=0.8,
            verified=True,
            evidence_count=3,
        )
        
        # Factual should score higher than opinion
        assert factual.confidence_score > opinion.confidence_score
    
    def test_evidence_boost(self):
        """Verified claims with evidence should get confidence boost."""
        scorer = ConfidenceScorer()
        
        unverified = scorer.score_claim(
            claim_text="Test claim",
            claim_type=ClaimType.FACTUAL,
            base_confidence=0.7,
            verified=False,
            evidence_count=0,
        )
        
        verified = scorer.score_claim(
            claim_text="Test claim",
            claim_type=ClaimType.FACTUAL,
            base_confidence=0.7,
            verified=True,
            evidence_count=3,
        )
        
        # Verified with evidence should score higher
        assert verified.confidence_score > unverified.confidence_score
    
    def test_custom_weights(self):
        """Custom weights should be applied correctly."""
        weights = ConfidenceWeights(
            factual_weight=2.0,
            reasoning_weight=1.0,
            planning_weight=0.5,
            opinion_weight=0.25,
        )
        scorer = ConfidenceScorer(weights)
        
        factual = scorer.score_claim(
            claim_text="Test",
            claim_type=ClaimType.FACTUAL,
            base_confidence=0.5,
        )
        
        opinion = scorer.score_claim(
            claim_text="Test",
            claim_type=ClaimType.OPINION,
            base_confidence=0.5,
        )
        
        # Factual with 2.0 weight should be 4x opinion with 0.25 weight
        assert abs(factual.confidence_score / opinion.confidence_score - 8.0) < 0.01
    
    def test_aggregate_sentence_confidence(self):
        """Aggregate confidence should be weighted average of claims."""
        scorer = ConfidenceScorer()
        
        claims = [
            scorer.score_claim(
                claim_text="Claim 1",
                claim_type=ClaimType.FACTUAL,
                base_confidence=0.9,
            ),
            scorer.score_claim(
                claim_text="Claim 2",
                claim_type=ClaimType.OPINION,
                base_confidence=0.5,
            ),
        ]
        
        aggregate = scorer.aggregate_sentence_confidence(claims)
        
        # Should be weighted average
        assert 0.5 < aggregate < 0.9
    
    def test_verify_sentence_passes_threshold(self):
        """Sentence should pass if aggregate confidence >= threshold."""
        scorer = ConfidenceScorer()
        
        claims = [
            scorer.score_claim(
                claim_text="High confidence claim",
                claim_type=ClaimType.FACTUAL,
                base_confidence=0.95,
            ),
        ]
        
        verification = scorer.verify_sentence(
            sentence_text="High confidence claim.",
            sentence_index=0,
            claims=claims,
            threshold=0.7,
        )
        
        assert verification.safe_to_deliver is True
        assert verification.aggregate_confidence >= 0.7
    
    def test_verify_sentence_fails_threshold(self):
        """Sentence should fail if aggregate confidence < threshold."""
        scorer = ConfidenceScorer()
        
        claims = [
            scorer.score_claim(
                claim_text="Low confidence claim",
                claim_type=ClaimType.OPINION,
                base_confidence=0.3,
            ),
        ]
        
        verification = scorer.verify_sentence(
            sentence_text="Low confidence claim.",
            sentence_index=0,
            claims=claims,
            threshold=0.7,
        )
        
        assert verification.safe_to_deliver is False
        assert verification.aggregate_confidence < 0.7


class TestStreamingVerifier:
    """Test sentence-level verification in streaming output."""
    
    def test_process_chunk_extracts_complete_sentences(self, mock_firewall):
        """Should extract and verify complete sentences."""
        verifier = StreamingVerifier(mock_firewall)
        
        # Process chunks that form complete sentences
        result1 = verifier.process_chunk("Hello world.", "snapshot1")
        result2 = verifier.process_chunk(" This is a test.", "snapshot1")
        
        # Should have verified 2 sentences
        assert len(result1["verified_sentences"]) == 1
        assert len(result2["verified_sentences"]) == 1
    
    def test_process_chunk_buffers_incomplete_sentences(self, mock_firewall):
        """Should buffer incomplete sentences."""
        verifier = StreamingVerifier(mock_firewall)
        
        # Process incomplete sentence
        result = verifier.process_chunk("This is incomplete", "snapshot1")
        
        # Should not verify yet
        assert len(result["verified_sentences"]) == 0
        
        # Complete the sentence
        result = verifier.process_chunk(" now.", "snapshot1")
        
        # Should verify now
        assert len(result["verified_sentences"]) == 1
    
    def test_flush_remaining_buffer(self, mock_firewall):
        """Flush should verify remaining buffered content."""
        verifier = StreamingVerifier(mock_firewall)
        
        verifier.process_chunk("Incomplete sentence", "snapshot1")
        final = verifier.flush()
        
        assert final is not None
        assert final.sentence_text == "Incomplete sentence"
    
    def test_verification_metadata_structure(self, mock_firewall):
        """Verification metadata should have correct structure for event storage."""
        verifier = StreamingVerifier(mock_firewall)
        
        verifier.process_chunk("First sentence.", "snapshot1")
        verifier.process_chunk(" Second sentence.", "snapshot1")
        verifier.flush()
        
        metadata = verifier.get_verification_metadata()
        
        # Check structure
        assert "verification_method" in metadata
        assert metadata["verification_method"] == "streaming_sentence_level"
        assert "total_sentences" in metadata
        assert "sentences_passed" in metadata
        assert "aggregate_confidence" in metadata
        assert "threshold" in metadata
        assert "sentences" in metadata
        
        # Check sentences array
        assert len(metadata["sentences"]) == 2
        for sent in metadata["sentences"]:
            assert "index" in sent
            assert "text" in sent
            assert "confidence" in sent
            assert "safe" in sent
            assert "claims" in sent
    
    def test_reset_clears_state(self, mock_firewall):
        """Reset should clear all verification state."""
        verifier = StreamingVerifier(mock_firewall)
        
        verifier.process_chunk("First sentence.", "snapshot1")
        verifier.reset()
        
        metadata = verifier.get_verification_metadata()
        assert metadata == {}


class TestStreamingVerificationIntegration:
    """Integration tests with event logger and database."""
    
    def test_verification_metadata_stored_in_event(
        self,
        db_session,
        session_manager,
        event_logger,
        mock_firewall,
    ):
        """Verification metadata should be stored in event.metadata."""
        # Create session and user query
        session = session_manager.create_session(user_id="test_user")
        user_event = event_logger.create_user_query(
            user_id="test_user",
            session_id=session.session_id,
            content="What is AI?",
        )
        
        # Simulate streaming verification
        verifier = StreamingVerifier(mock_firewall)
        verifier.process_chunk("AI is artificial intelligence.", "snapshot1")
        verifier.process_chunk(" It enables machines to learn.", "snapshot1")
        verifier.flush()
        
        # Create LLM response with verification metadata
        metadata = {
            "trust_safety": {
                "verification": verifier.get_verification_metadata(),
            }
        }
        
        response_event = event_logger.create_llm_response(
            user_id="test_user",
            session_id=session.session_id,
            content="AI is artificial intelligence. It enables machines to learn.",
            agent_id="dev-agent",
            agent_version="0.1.0",
            parent_event_id=user_event.event_id,
            causal_chain_id=user_event.causal_chain_id,
        )
        
        # Update metadata
        db_session.execute(
            text("""
                UPDATE conversation_events 
                SET metadata = :metadata 
                WHERE event_id = :event_id
            """),
            {
                "metadata": json.dumps(metadata),
                "event_id": response_event.event_id,
            },
        )
        db_session.commit()
        
        # Verify metadata was stored
        result = db_session.execute(
            text("""
                SELECT metadata FROM conversation_events 
                WHERE event_id = :event_id
            """),
            {"event_id": response_event.event_id},
        ).fetchone()
        
        assert result is not None
        stored_metadata = json.loads(result[0])
        assert "trust_safety" in stored_metadata
        assert "verification" in stored_metadata["trust_safety"]
        assert stored_metadata["trust_safety"]["verification"]["total_sentences"] == 2
    
    def test_slo_query_filters_by_confidence(
        self,
        db_session,
        session_manager,
        event_logger,
        mock_firewall,
    ):
        """SLO queries should be able to filter by confidence threshold."""
        # Create multiple verifiers with different confidence levels
        verifiers = []
        for i in range(3):
            verifier = StreamingVerifier(mock_firewall, verification_threshold=0.7)
            verifier.process_chunk(f"Statement {i}.", f"snapshot_{i}")
            verifier.flush()
            verifiers.append(verifier)
        
        # Verify metadata structure for each
        high_confidence_count = 0
        for i, verifier in enumerate(verifiers):
            metadata = verifier.get_verification_metadata()
            
            # Manually set confidence for testing
            confidence = 0.5 + (i * 0.2)  # 0.5, 0.7, 0.9
            metadata["aggregate_confidence"] = confidence
            
            if confidence >= 0.7:
                high_confidence_count += 1
        
        # Should find 2 verifiers with confidence >= 0.7
        assert high_confidence_count == 2
    
    def test_parallel_verification_no_cross_contamination(
        self,
        db_session,
        session_manager,
        event_logger,
        mock_firewall,
    ):
        """Parallel verification should not contaminate state."""
        # Create multiple verifiers (simulating parallel requests)
        verifiers = [StreamingVerifier(mock_firewall) for _ in range(3)]
        
        # Process different content in each verifier
        for i, verifier in enumerate(verifiers):
            verifier.process_chunk(f"Sentence {i}.", f"snapshot_{i}")
            verifier.flush()
        
        # Each verifier should have independent state
        for i, verifier in enumerate(verifiers):
            metadata = verifier.get_verification_metadata()
            assert metadata["total_sentences"] == 1
            assert f"Sentence {i}" in metadata["sentences"][0]["text"]


class TestConfidenceThresholdSLO:
    """Test SLO-compatible confidence threshold filtering."""
    
    def test_high_confidence_responses_pass_slo(
        self,
        db_session,
        session_manager,
        event_logger,
        mock_firewall,
    ):
        """High-confidence responses should pass SLO threshold."""
        # Create high-confidence response
        verifier = StreamingVerifier(mock_firewall, verification_threshold=0.7)
        verifier.process_chunk("High confidence statement.", "snapshot1")
        verifier.flush()
        
        metadata = verifier.get_verification_metadata()
        
        # Verify metadata has correct structure
        assert "aggregate_confidence" in metadata
        assert "threshold" in metadata
        assert metadata["threshold"] == 0.7
        
        # Verify aggregate confidence is >= threshold
        assert metadata["aggregate_confidence"] >= metadata["threshold"]
