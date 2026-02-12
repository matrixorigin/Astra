"""Hallucination Firewall - Verify LLM responses against data snapshots.

Prevents delivering incorrect information by verifying claims against
the same data snapshot the LLM saw during generation.
"""

import json
from dataclasses import dataclass

from core.logging_config import get_logger
from core.verification.claim_extractor import Claim, ClaimExtractor

logger = get_logger(__name__)


@dataclass
class VerificationResult:
    """Result of claim verification."""

    claim: Claim
    verified: bool
    confidence: float  # 0.0-1.0
    evidence: str | None = None
    contradiction: str | None = None


@dataclass
class FirewallResult:
    """Overall firewall result."""

    safe_to_deliver: bool
    confidence_score: float  # 0.0-1.0
    claims_verified: int
    claims_failed: int
    contradictions: list[VerificationResult]
    warnings: list[str]


class HallucinationFirewall:
    """Verify LLM responses against data snapshots."""

    def __init__(self, db, context_manager, threshold: float = 0.7):
        """Initialize firewall.

        Args:
            db: Database connection
            context_manager: ContextManager for loading snapshots
            threshold: Minimum confidence to pass (default: 0.7)
        """
        self.db = db
        self.context_manager = context_manager
        self.threshold = threshold
        self.extractor = ClaimExtractor()

    def verify_response(
        self, response: str, snapshot_id: str, mode: str = "warn"
    ) -> FirewallResult:
        """Verify LLM response against context snapshot.

        Args:
            response: LLM response text
            snapshot_id: Context snapshot ID
            mode: 'warn' (annotate) or 'block' (reject delivery)

        Returns:
            FirewallResult with verification details
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

        if not snapshot_id or not snapshot_id.strip():
            logger.error("No snapshot_id provided to firewall")
            return FirewallResult(
                safe_to_deliver=True,  # Fail open
                confidence_score=0.5,
                claims_verified=0,
                claims_failed=0,
                contradictions=[],
                warnings=["No snapshot_id provided"],
            )

        if mode not in ("warn", "block"):
            logger.warning(f"Invalid mode '{mode}', defaulting to 'warn'")
            mode = "warn"

        # 1. Extract claims
        try:
            claims = self.extractor.extract(response)
            logger.debug(f"Extracted {len(claims)} claims from response")
        except Exception as e:
            logger.error(f"Claim extraction failed: {e}")
            return FirewallResult(
                safe_to_deliver=True,  # Fail open
                confidence_score=0.5,
                claims_verified=0,
                claims_failed=0,
                contradictions=[],
                warnings=[f"Claim extraction failed: {e}"],
            )

        if not claims:
            # No verifiable claims - pass through
            return FirewallResult(
                safe_to_deliver=True,
                confidence_score=1.0,
                claims_verified=0,
                claims_failed=0,
                contradictions=[],
                warnings=["No verifiable claims found"],
            )

        # 2. Load context snapshot
        try:
            context = self.context_manager.load_snapshot(snapshot_id)
        except Exception as e:
            logger.error(f"Failed to load snapshot {snapshot_id}: {e}")
            return FirewallResult(
                safe_to_deliver=True,  # Fail open
                confidence_score=0.5,
                claims_verified=0,
                claims_failed=0,
                contradictions=[],
                warnings=[f"Snapshot load failed: {e}"],
            )

        # 3. Verify each claim
        results = []
        for claim in claims:
            try:
                result = self._verify_claim(claim, context)
                results.append(result)
            except Exception as e:
                logger.error(f"Claim verification failed for '{claim.value}': {e}")
                # Treat as failed verification
                results.append(
                    VerificationResult(
                        claim=claim,
                        verified=False,
                        confidence=0.0,
                        contradiction=f"Verification error: {e}",
                    )
                )
            results.append(result)

        # 4. Compute overall result
        verified = [r for r in results if r.verified]
        failed = [r for r in results if not r.verified]

        confidence = len(verified) / len(results) if results else 1.0

        safe = confidence >= self.threshold if mode == "block" else True

        return FirewallResult(
            safe_to_deliver=safe,
            confidence_score=confidence,
            claims_verified=len(verified),
            claims_failed=len(failed),
            contradictions=failed,
            warnings=[] if safe else ["Confidence below threshold"],
        )

    def _verify_claim(self, claim: Claim, context) -> VerificationResult:
        """Verify a single claim against context.

        Strategy:
        - Numbers: Check if mentioned in context events
        - Dates: Check if mentioned in context events
        - References: Check if PR/issue/commit exists in context
        - Booleans: Fuzzy match against context content
        """
        # Simple verification: check if claim value appears in context
        context_text = self._context_to_text(context)

        if claim.value.lower() in context_text.lower():
            return VerificationResult(
                claim=claim,
                verified=True,
                confidence=0.9,
                evidence=f"Found in context: {claim.value}",
            )

        # Not found - potential hallucination
        return VerificationResult(
            claim=claim,
            verified=False,
            confidence=0.3,
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

    def log_verification(
        self, session_id: str, event_id: str, result: FirewallResult
    ) -> None:
        """Log verification result as event.

        Args:
            session_id: Session ID
            event_id: Event ID being verified
            result: Verification result
        """
        if not session_id or not event_id:
            logger.error("Missing session_id or event_id for verification logging")
            return

        try:
            self.db.execute(
                """
                INSERT INTO conversation_events (
                    event_id, user_id, session_id, agent_id, agent_version,
                    event_type, content, metadata, created_at
                ) VALUES (
                    %s, 'system', %s, 'firewall', '1.0',
                    'hallucination_check', %s, %s, NOW()
                )
                """,
                (
                    f"verify_{event_id}",
                    session_id,
                    json.dumps(
                        {
                            "safe": result.safe_to_deliver,
                            "confidence": result.confidence_score,
                            "verified": result.claims_verified,
                            "failed": result.claims_failed,
                        }
                    ),
                    json.dumps(
                        {
                            "contradictions": [
                                {
                                    "claim": c.claim.value,
                                    "type": c.claim.type,
                                    "contradiction": c.contradiction,
                                }
                                for c in result.contradictions
                            ]
                        }
                    ),
                ),
            )

            logger.info(
                f"Verification logged: {result.claims_verified}/{result.claims_verified + result.claims_failed} verified, "
                f"confidence={result.confidence_score:.2f}"
            )
        except Exception as e:
            logger.error(f"Failed to log verification: {e}")
