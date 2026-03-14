"""Sentence-level verification for streaming output — P0 Trust & Safety.

Hooks into chat_loop TEXT_DELTA events to verify sentences as they stream,
storing verification metadata in event audit trail.
"""

from __future__ import annotations

import json
import re
import time
from typing import Any

from core.logging_config import get_logger
from core.trust_safety.confidence_scorer import (
    ClaimType,
    ConfidenceScorer,
    ConfidenceWeights,
    SentenceVerification,
)

logger = get_logger(__name__)


class StreamingVerifier:
    """Verify sentences in streaming output and store verification metadata."""

    def __init__(
        self,
        firewall,
        confidence_scorer: ConfidenceScorer | None = None,
        verification_threshold: float = 0.7,
    ):
        """Initialize streaming verifier.

        Args:
            firewall: HallucinationFirewall instance
            confidence_scorer: Custom confidence scorer (default: standard)
            verification_threshold: Minimum confidence to pass (default: 0.7)
        """
        self.firewall = firewall
        self.scorer = confidence_scorer or ConfidenceScorer()
        self.threshold = verification_threshold
        self._sentence_buffer = ""
        self._sentence_index = 0
        self._verifications: list[SentenceVerification] = []

    def process_chunk(
        self,
        chunk: str,
        context_snapshot_id: str,
        model_used: str = "",
    ) -> dict[str, Any]:
        """Process a streaming chunk and verify complete sentences.

        Args:
            chunk: Text chunk from streaming output
            context_snapshot_id: Context snapshot ID for verification
            model_used: Model that generated the chunk

        Returns:
            Dict with verification results for complete sentences
        """
        self._sentence_buffer += chunk
        verified_sentences = []

        # Split on sentence boundaries (., !, ?)
        sentences = re.split(r"(?<=[.!?])\s+", self._sentence_buffer)

        # Keep last incomplete sentence in buffer
        if not re.search(r"[.!?]\s*$", self._sentence_buffer):
            self._sentence_buffer = sentences[-1] if sentences else ""
            sentences = sentences[:-1]
        else:
            self._sentence_buffer = ""

        # Verify complete sentences
        for sentence in sentences:
            if not sentence.strip():
                continue

            verification = self._verify_sentence(
                sentence,
                context_snapshot_id,
                model_used,
            )
            verified_sentences.append(verification)
            self._verifications.append(verification)

        return {
            "verified_sentences": [
                {
                    "sentence": v.sentence_text,
                    "index": v.sentence_index,
                    "confidence": v.aggregate_confidence,
                    "safe": v.safe_to_deliver,
                    "claims_count": len(v.claims),
                }
                for v in verified_sentences
            ],
            "aggregate_confidence": self._aggregate_all_confidence(),
        }

    def flush(self) -> SentenceVerification | None:
        """Flush remaining buffered sentence.

        Returns:
            Final SentenceVerification if buffer has content
        """
        if not self._sentence_buffer.strip():
            return None

        verification = self._verify_sentence(
            self._sentence_buffer,
            "",
            "",
        )
        self._verifications.append(verification)
        self._sentence_buffer = ""
        return verification

    def get_verification_metadata(self) -> dict[str, Any]:
        """Get verification metadata for event storage.

        Returns:
            Dict suitable for storing in event.metadata["trust_safety.verification"]
        """
        if not self._verifications:
            return {}

        return {
            "verification_method": "streaming_sentence_level",
            "total_sentences": len(self._verifications),
            "sentences_passed": sum(1 for v in self._verifications if v.safe_to_deliver),
            "aggregate_confidence": self._aggregate_all_confidence(),
            "threshold": self.threshold,
            "sentences": [
                {
                    "index": v.sentence_index,
                    "text": v.sentence_text[:100],  # Truncate for storage
                    "confidence": v.aggregate_confidence,
                    "safe": v.safe_to_deliver,
                    "claims": [
                        {
                            "text": c.claim_text[:50],
                            "type": c.claim_type.value,
                            "confidence": c.confidence_score,
                            "verified": c.verified,
                            "evidence_count": c.evidence_count,
                        }
                        for c in v.claims
                    ],
                }
                for v in self._verifications
            ],
        }

    def reset(self) -> None:
        """Reset verifier state for next response."""
        self._sentence_buffer = ""
        self._sentence_index = 0
        self._verifications = []

    # Private methods

    def _verify_sentence(
        self,
        sentence: str,
        context_snapshot_id: str,
        model_used: str,
    ) -> SentenceVerification:
        """Verify a single sentence.

        Args:
            sentence: Sentence text
            context_snapshot_id: Context snapshot ID
            model_used: Model that generated the sentence

        Returns:
            SentenceVerification with confidence scores
        """
        # Extract claims from sentence (simplified: use firewall's extractor)
        claims = []

        try:
            # Use firewall's LLM extractor if available
            if self.firewall.llm_extractor:
                extracted = self.firewall.llm_extractor.extract_claims(sentence)
                for claim in extracted:
                    # Classify claim type (simplified heuristic)
                    claim_type = self._classify_claim_type(claim.text)

                    # Score the claim
                    scored = self.scorer.score_claim(
                        claim_text=claim.text,
                        claim_type=claim_type,
                        base_confidence=claim.confidence,
                        verification_method="llm_extraction",
                        verified=claim.verified,
                        evidence_count=len(claim.evidence) if hasattr(claim, "evidence") else 0,
                        context_snapshot_id=context_snapshot_id,
                        model_used=model_used,
                        timestamp=time.time(),
                    )
                    claims.append(scored)
        except Exception as e:
            logger.warning("Claim extraction failed (non-fatal): %s", e)

        # Verify sentence
        return self.scorer.verify_sentence(
            sentence_text=sentence,
            sentence_index=self._sentence_index,
            claims=claims,
            threshold=self.threshold,
        )

    def _classify_claim_type(self, claim_text: str) -> ClaimType:
        """Classify claim type using heuristics.

        Args:
            claim_text: Claim text

        Returns:
            ClaimType classification
        """
        lower = claim_text.lower()

        # Planning: future tense, action words
        if any(
            w in lower for w in ["will", "should", "can", "could", "may", "might", "plan", "intend"]
        ):
            return ClaimType.PLANNING

        # Reasoning: logical connectors
        if any(w in lower for w in ["because", "therefore", "thus", "hence", "so", "since"]):
            return ClaimType.REASONING

        # Opinion: subjective markers
        if any(
            w in lower for w in ["think", "believe", "feel", "seem", "appear", "opinion", "view"]
        ):
            return ClaimType.OPINION

        # Default: factual
        return ClaimType.FACTUAL

    def _aggregate_all_confidence(self) -> float:
        """Aggregate confidence across all verified sentences.

        Returns:
            Overall confidence score (0.0-1.0)
        """
        if not self._verifications:
            return 1.0

        total = sum(v.aggregate_confidence for v in self._verifications)
        return total / len(self._verifications)
