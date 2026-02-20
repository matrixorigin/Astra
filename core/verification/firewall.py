"""Hallucination Firewall - Verify LLM responses against data snapshots.

Enhanced version with:
- LLM-based claim extraction (catches implicit assertions)
- Structured verification against snapshot content
- Evidence backlinks for traceability
"""

import json
from dataclasses import dataclass

from core.logging_config import get_logger
from core.verification.claim_extractor import ClaimExtractor
from core.verification.llm_claim_extractor import LLMClaimExtractor, Claim
from core.verification.structured_verifier import (
    StructuredVerifier,
    VerificationResult,
)

logger = get_logger(__name__)


@dataclass
class FirewallResult:
    """Overall firewall result."""

    safe_to_deliver: bool
    confidence_score: float  # 0.0-1.0
    claims_verified: int
    claims_failed: int
    contradictions: list[VerificationResult]
    warnings: list[str]
    evidence_count: int = 0  # Total evidence pieces found


class HallucinationFirewall:
    """Verify LLM responses against data snapshots.

    Enhanced with:
    - LLM-based claim extraction (vs regex)
    - Structured verification with evidence backlinks
    - Semantic confidence scoring
    """

    def __init__(
        self,
        db,
        context_manager,
        llm_client=None,
        threshold: float = 0.7,
        use_llm_extraction: bool = True,
    ):
        """Initialize firewall.

        Args:
            db: SQLAlchemy Session
            context_manager: ContextManager for loading snapshots
            llm_client: LLM client for enhanced extraction/verification
            threshold: Minimum confidence to pass (default: 0.7)
            use_llm_extraction: Use LLM for claim extraction (default: True)
        """
        self.db = db
        self.context_manager = context_manager
        self.threshold = threshold
        self.use_llm_extraction = use_llm_extraction

        # Extractors
        self.regex_extractor = ClaimExtractor()
        self.llm_extractor = LLMClaimExtractor(llm_client) if llm_client else None

        # Verifier
        self.verifier = StructuredVerifier(llm_client) if llm_client else None

        # Initialize tables (auto-create if not exist)
        self._init_tables()

    def verify_response(
        self, response: str, context_capture_id: str, mode: str = "warn"
    ) -> FirewallResult:
        """Verify LLM response against context capture.

        Enhanced with:
        - LLM-based claim extraction (if enabled)
        - Structured verification with evidence backlinks
        - Semantic confidence scoring

        Args:
            response: LLM response text
            context_capture_id: Context capture ID (business-level, not MatrixOne snapshot)
            mode: 'warn' (annotate) or 'block' (reject delivery)

        Returns:
            FirewallResult with verification details and evidence
        """
        # Input validation
        if not response or not response.strip():
            logger.warning("Empty response provided to firewall")
            return FirewallResult(
                safe_to_deliver=True,
                confidence_score=1.0,
                claims_verified=0,
                claims_failed=0,
                contradictions=[],
                warnings=["Empty response"],
            )

        if not context_capture_id or not context_capture_id.strip():
            logger.error("No context_capture_id provided to firewall")
            return self._degraded_result(mode, "No context_capture_id provided")

        if mode not in ("warn", "block"):
            logger.warning(f"Invalid mode '{mode}', defaulting to 'warn'")
            mode = "warn"

        # 1. Extract claims (LLM or regex)
        try:
            if self.use_llm_extraction and self.llm_extractor:
                claims = self.llm_extractor.extract(response)
                logger.info(f"Extracted {len(claims)} claims using LLM")
            else:
                claims = self.regex_extractor.extract(response)
                logger.info(f"Extracted {len(claims)} claims using regex")
        except Exception as e:
            logger.error(f"Claim extraction failed: {e}")
            return self._degraded_result(mode, f"Claim extraction failed: {e}")

        if not claims:
            return FirewallResult(
                safe_to_deliver=True,
                confidence_score=1.0,
                claims_verified=0,
                claims_failed=0,
                contradictions=[],
                warnings=["No verifiable claims found"],
            )

        # 2. Load context capture
        try:
            snapshot = self.context_manager.load_snapshot(context_capture_id)
        except Exception as e:
            logger.error(f"Failed to load context capture {context_capture_id}: {e}")
            return self._degraded_result(mode, f"Context capture load failed: {e}")

        # 3. Verify each claim (structured or simple)
        results = []
        total_evidence = 0

        for claim in claims:
            try:
                if self.verifier:
                    # Structured verification with evidence
                    result = self.verifier.verify_claim(claim, snapshot)
                    total_evidence += len(result.evidence)
                else:
                    # Fallback to simple verification
                    result = self._simple_verify_claim(claim, snapshot)

                results.append(result)
            except Exception as e:
                logger.error(f"Claim verification failed for '{claim.value}': {e}")
                results.append(
                    VerificationResult(
                        claim=claim,
                        verified=False,
                        confidence=0.0,
                        evidence=[],
                        contradiction=f"Verification error: {e}",
                    )
                )

        # 4. Compute overall result — weighted by claim type risk
        verified = [r for r in results if r.verified]
        failed = [r for r in results if not r.verified]

        confidence = self._weighted_confidence(results) if results else 1.0

        safe = confidence >= self.threshold if mode == "block" else True

        return FirewallResult(
            safe_to_deliver=safe,
            confidence_score=confidence,
            claims_verified=len(verified),
            claims_failed=len(failed),
            contradictions=failed,
            warnings=[] if safe else ["Confidence below threshold"],
            evidence_count=total_evidence,
        )

    def _simple_verify_claim(self, claim: Claim, snapshot) -> VerificationResult:
        """Verification fallback: substring match → embedding similarity.

        Uses embedding similarity when available to catch paraphrases
        that substring matching misses.
        """
        context_text = self._context_to_text(snapshot)

        # Fast path: exact substring match
        if claim.value.lower() in context_text.lower():
            return VerificationResult(
                claim=claim,
                verified=True,
                confidence=0.9,
                evidence=[],
                contradiction=None,
            )

        # Slow path: embedding similarity (if available)
        try:
            from core.context.embeddings import EmbeddingService
            svc = EmbeddingService(self.db)
            claim_vec = svc.embed_text(claim.value)
            ctx_vec = svc.embed_text(context_text[:2000])  # cap context length
            dot = sum(a * b for a, b in zip(claim_vec, ctx_vec))
            norm_a = sum(a * a for a in claim_vec) ** 0.5
            norm_b = sum(b * b for b in ctx_vec) ** 0.5
            sim = dot / (norm_a * norm_b) if norm_a and norm_b else 0.0
            if sim >= 0.75:
                return VerificationResult(
                    claim=claim,
                    verified=True,
                    confidence=round(sim, 2),
                    evidence=[],
                    contradiction=None,
                )
        except Exception:
            pass  # No embedding service — fall through to unverified

        return VerificationResult(
            claim=claim,
            verified=False,
            confidence=0.3,
            evidence=[],
            contradiction=f"Claim '{claim.value}' not found in context",
        )

    def _context_to_text(self, context) -> str:
        """Convert context snapshot to searchable text."""
        parts = [context.system_prompt]

        for event in context.selected_events:
            parts.append(event.get("content", ""))

        for code in context.code_context:
            parts.append(code.get("content", ""))

        return "\n".join(parts)

    @staticmethod
    def _degraded_result(mode: str, reason: str) -> FirewallResult:
        """Return degraded result: fail-open in warn mode, fail-closed in block mode."""
        return FirewallResult(
            safe_to_deliver=(mode != "block"),
            confidence_score=0.0 if mode == "block" else 0.5,
            claims_verified=0,
            claims_failed=0,
            contradictions=[],
            warnings=[reason],
        )

    # Claim type → risk weight (higher = more dangerous when failed)
    _CLAIM_WEIGHTS: dict[str, float] = {
        "causal": 1.0, "factual": 0.8, "temporal": 0.6, "numeric": 0.5,
        # regex extractor types (mapped to equivalent weights)
        "reference": 0.8, "boolean": 0.8, "number": 0.5, "date": 0.6,
    }

    @classmethod
    def _weighted_confidence(cls, results: list) -> float:
        """Compute confidence weighted by claim type risk."""
        w_verified = 0.0
        w_total = 0.0
        for r in results:
            w = cls._CLAIM_WEIGHTS.get(getattr(r.claim, "type", ""), 0.7)
            w_total += w
            if r.verified:
                w_verified += w
        return w_verified / w_total if w_total > 0 else 1.0

    def log_verification(
        self, session_id: str, event_id: str, result: FirewallResult, context_capture_id: str
    ) -> None:
        """Log verification result with evidence backlinks.

        Args:
            session_id: Session ID
            event_id: Event ID being verified
            result: Verification result
            context_capture_id: Context capture ID (business-level)
        """
        if not session_id or not event_id:
            logger.error("Missing session_id or event_id for verification logging")
            return

        try:
            from sqlalchemy import text

            # 1. Insert hallucination check record
            check_id = f"check_{event_id}"

            self.db.execute(
                text(
                    "INSERT INTO hallucination_checks "
                    "(check_id, session_id, event_id, context_capture_id, "
                    "claims_total, claims_verified, claims_contradicted, "
                    "confidence_score, safe_to_deliver, evidence_count, created_at) "
                    "VALUES (:check_id, :session_id, :event_id, :ctx_id, "
                    ":total, :verified, :contradicted, :confidence, :safe, :evidence, NOW())"
                ),
                {
                    "check_id": check_id,
                    "session_id": session_id,
                    "event_id": event_id,
                    "ctx_id": context_capture_id,
                    "total": result.claims_verified + result.claims_failed,
                    "verified": result.claims_verified,
                    "contradicted": result.claims_failed,
                    "confidence": result.confidence_score,
                    "safe": result.safe_to_deliver,
                    "evidence": result.evidence_count,
                },
            )

            # 2. Insert evidence backlinks for each contradiction
            for contradiction in result.contradictions:
                for evidence in contradiction.evidence:
                    self.db.execute(
                        text(
                            "INSERT INTO claim_evidence "
                            "(check_id, claim_type, claim_value, "
                            "source_type, source_id, content, location, "
                            "confidence, created_at) "
                            "VALUES (:check_id, :claim_type, :claim_value, "
                            ":source_type, :source_id, :content, :location, "
                            ":confidence, NOW())"
                        ),
                        {
                            "check_id": check_id,
                            "claim_type": contradiction.claim.type,
                            "claim_value": contradiction.claim.value,
                            "source_type": evidence.source_type,
                            "source_id": evidence.source_id,
                            "content": evidence.content,
                            "location": evidence.location,
                            "confidence": evidence.confidence,
                        },
                    )

            self.db.commit()

            logger.info(
                f"Verification logged: {result.claims_verified}/{result.claims_verified + result.claims_failed} verified, "
                f"confidence={result.confidence_score:.2f}, evidence={result.evidence_count}"
            )

        except Exception as e:
            logger.error(f"Failed to log verification: {e}")
            self.db.rollback()

    def _init_tables(self):
        """Initialize database tables (auto-create if not exist)."""
        try:
            from core.verification.schema import init_hallucination_tables

            init_hallucination_tables(self.db)
        except Exception as e:
            logger.warning(f"Table initialization skipped: {e}")
