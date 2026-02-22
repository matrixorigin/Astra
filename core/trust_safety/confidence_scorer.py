"""Weighted confidence scoring for claims — P0 Trust & Safety.

Extends firewall with:
- Claim-type-specific confidence weights
- Sentence-level verification metadata
- Confidence aggregation for streaming output
- SLO-compatible signal source
"""

from __future__ import annotations

from dataclasses import dataclass, field
from enum import Enum


class ClaimType(str, Enum):
    """Claim classification for weighted scoring."""
    FACTUAL = "factual"  # Verifiable facts (highest weight)
    REASONING = "reasoning"  # Logical inference (medium weight)
    PLANNING = "planning"  # Future actions (lower weight)
    OPINION = "opinion"  # Subjective statements (lowest weight)


@dataclass
class ClaimConfidence:
    """Confidence score for a single claim."""
    claim_text: str
    claim_type: ClaimType
    confidence_score: float  # 0.0-1.0
    verification_method: str  # "llm", "retrieval", "hybrid", "none"
    verified: bool
    evidence_count: int = 0
    timestamp: float = 0.0  # Unix timestamp
    context_snapshot_id: str = ""
    model_used: str = ""


@dataclass
class SentenceVerification:
    """Verification result for a sentence in streaming output."""
    sentence_text: str
    sentence_index: int
    claims: list[ClaimConfidence] = field(default_factory=list)
    aggregate_confidence: float = 0.0  # Weighted average of claims
    safe_to_deliver: bool = True
    verification_timestamp: float = 0.0


class ConfidenceWeights:
    """Configurable weights for claim types."""
    
    def __init__(
        self,
        factual_weight: float = 1.0,
        reasoning_weight: float = 0.8,
        planning_weight: float = 0.6,
        opinion_weight: float = 0.4,
    ):
        """Initialize confidence weights.
        
        Args:
            factual_weight: Weight for factual claims (default: 1.0)
            reasoning_weight: Weight for reasoning claims (default: 0.8)
            planning_weight: Weight for planning claims (default: 0.6)
            opinion_weight: Weight for opinion claims (default: 0.4)
        """
        self.weights = {
            ClaimType.FACTUAL: factual_weight,
            ClaimType.REASONING: reasoning_weight,
            ClaimType.PLANNING: planning_weight,
            ClaimType.OPINION: opinion_weight,
        }
    
    def get_weight(self, claim_type: ClaimType) -> float:
        """Get weight for claim type."""
        return self.weights.get(claim_type, 0.5)


class ConfidenceScorer:
    """Score and aggregate confidence for claims and sentences."""
    
    def __init__(self, weights: ConfidenceWeights | None = None):
        """Initialize scorer.
        
        Args:
            weights: Custom confidence weights (default: standard weights)
        """
        self.weights = weights or ConfidenceWeights()
    
    def score_claim(
        self,
        claim_text: str,
        claim_type: ClaimType,
        base_confidence: float,
        verification_method: str = "none",
        verified: bool = False,
        evidence_count: int = 0,
        context_snapshot_id: str = "",
        model_used: str = "",
        timestamp: float = 0.0,
    ) -> ClaimConfidence:
        """Score a single claim with type-specific weighting.
        
        Args:
            claim_text: The claim statement
            claim_type: Type of claim (factual/reasoning/planning/opinion)
            base_confidence: Base confidence score (0.0-1.0)
            verification_method: How it was verified
            verified: Whether verification passed
            evidence_count: Number of evidence pieces found
            context_snapshot_id: Linked context snapshot
            model_used: Model that generated the claim
            timestamp: When the claim was generated
        
        Returns:
            ClaimConfidence with weighted score
        """
        weight = self.weights.get_weight(claim_type)
        weighted_score = base_confidence * weight
        
        # Boost confidence if verified with evidence
        if verified and evidence_count > 0:
            evidence_boost = min(0.1, evidence_count * 0.02)
            weighted_score = min(1.0, weighted_score + evidence_boost)
        
        return ClaimConfidence(
            claim_text=claim_text,
            claim_type=claim_type,
            confidence_score=weighted_score,
            verification_method=verification_method,
            verified=verified,
            evidence_count=evidence_count,
            timestamp=timestamp,
            context_snapshot_id=context_snapshot_id,
            model_used=model_used,
        )
    
    def aggregate_sentence_confidence(
        self,
        claims: list[ClaimConfidence],
        threshold: float = 0.7,
    ) -> float:
        """Aggregate confidence scores for a sentence.
        
        Args:
            claims: List of claims in the sentence
            threshold: Minimum confidence to pass (default: 0.7)
        
        Returns:
            Aggregate confidence score (0.0-1.0)
        """
        if not claims:
            return 1.0  # No claims = safe
        
        # Weighted average of claim confidences
        total_weight = sum(self.weights.get_weight(c.claim_type) for c in claims)
        if total_weight == 0:
            return 1.0
        
        weighted_sum = sum(
            c.confidence_score * self.weights.get_weight(c.claim_type)
            for c in claims
        )
        return weighted_sum / total_weight
    
    def verify_sentence(
        self,
        sentence_text: str,
        sentence_index: int,
        claims: list[ClaimConfidence],
        threshold: float = 0.7,
    ) -> SentenceVerification:
        """Verify a sentence and aggregate claim confidences.
        
        Args:
            sentence_text: The sentence to verify
            sentence_index: Position in the output stream
            claims: Claims extracted from the sentence
            threshold: Minimum confidence to pass
        
        Returns:
            SentenceVerification with aggregate confidence
        """
        aggregate = self.aggregate_sentence_confidence(claims, threshold)
        safe = aggregate >= threshold
        
        return SentenceVerification(
            sentence_text=sentence_text,
            sentence_index=sentence_index,
            claims=claims,
            aggregate_confidence=aggregate,
            safe_to_deliver=safe,
        )
