"""Structured verification against snapshot content.

Verifies claims against specific parts of the context snapshot,
providing evidence backlinks for traceability.
"""

import json
from dataclasses import dataclass

from core.logging_config import get_logger
from core.verification.llm_claim_extractor import Claim

logger = get_logger(__name__)


@dataclass
class Evidence:
    """Evidence supporting or contradicting a claim."""

    source_type: str  # 'event' | 'code' | 'doc' | 'skill'
    source_id: str  # event_id, file_path, etc.
    content: str  # Relevant excerpt
    location: str  # Line number, position, etc.
    confidence: float  # 0.0-1.0


@dataclass
class VerificationResult:
    """Result of structured claim verification."""

    claim: Claim
    verified: bool
    confidence: float  # 0.0-1.0
    evidence: list[Evidence]  # Supporting evidence
    contradiction: str | None = None


class StructuredVerifier:
    """Verify claims against structured snapshot content."""

    def __init__(self, llm_client):
        """Initialize verifier.

        Args:
            llm_client: LLM client for semantic verification
        """
        self.llm_client = llm_client

    def verify_claim(self, claim: Claim, snapshot) -> VerificationResult:
        """Verify a claim against snapshot content.

        Args:
            claim: Claim to verify
            snapshot: Context snapshot with structured content

        Returns:
            VerificationResult with evidence backlinks
        """
        evidence = []

        # 1. Search in events
        event_evidence = self._search_events(claim, snapshot.selected_events)
        evidence.extend(event_evidence)

        # 2. Search in code context
        code_evidence = self._search_code(claim, snapshot.code_context)
        evidence.extend(code_evidence)

        # 3. Search in documentation
        doc_evidence = self._search_docs(claim, snapshot.documentation)
        evidence.extend(doc_evidence)

        # 4. Compute verification result
        if not evidence:
            return VerificationResult(
                claim=claim,
                verified=False,
                confidence=0.0,
                evidence=[],
                contradiction=f"No evidence found for claim: {claim.value}",
            )

        # Use LLM for semantic verification
        verified, confidence = self._semantic_verify(claim, evidence)

        return VerificationResult(
            claim=claim,
            verified=verified,
            confidence=confidence,
            evidence=evidence,
            contradiction=None if verified else "Semantic verification failed",
        )

    def _search_events(self, claim: Claim, events: list) -> list[Evidence]:
        """Search for evidence in conversation events."""
        evidence = []

        for event in events:
            content = event.get("content", "")
            if not content:
                continue

            # Check for exact match
            if claim.value.lower() in content.lower():
                evidence.append(
                    Evidence(
                        source_type="event",
                        source_id=event.get("event_id", "unknown"),
                        content=self._extract_context(content, claim.value),
                        location=f"event:{event.get('event_id', 'unknown')}",
                        confidence=0.9,
                    )
                )

            # Check for semantic match (for numeric/temporal claims)
            elif claim.type in ("numeric", "temporal"):
                semantic_match = self._fuzzy_match(claim.value, content)
                if semantic_match:
                    evidence.append(
                        Evidence(
                            source_type="event",
                            source_id=event.get("event_id", "unknown"),
                            content=semantic_match,
                            location=f"event:{event.get('event_id', 'unknown')}",
                            confidence=0.7,
                        )
                    )

        return evidence

    def _search_code(self, claim: Claim, code_context: list) -> list[Evidence]:
        """Search for evidence in code context."""
        evidence = []

        for code in code_context:
            content = code.get("content", "")
            file_path = code.get("file_path", "unknown")

            if claim.value.lower() in content.lower():
                # Find line number
                lines = content.split("\n")
                line_num = 0
                for i, line in enumerate(lines, 1):
                    if claim.value.lower() in line.lower():
                        line_num = i
                        break

                evidence.append(
                    Evidence(
                        source_type="code",
                        source_id=file_path,
                        content=self._extract_context(content, claim.value),
                        location=f"{file_path}:{line_num}",
                        confidence=0.95,
                    )
                )

        return evidence

    def _search_docs(self, claim: Claim, documentation: list) -> list[Evidence]:
        """Search for evidence in documentation."""
        evidence = []

        for doc in documentation:
            content = doc.get("content", "")
            doc_id = doc.get("doc_id", "unknown")

            if claim.value.lower() in content.lower():
                evidence.append(
                    Evidence(
                        source_type="doc",
                        source_id=doc_id,
                        content=self._extract_context(content, claim.value),
                        location=f"doc:{doc_id}",
                        confidence=0.85,
                    )
                )

        return evidence

    def _extract_context(self, text: str, value: str, window: int = 100) -> str:
        """Extract context around a value in text."""
        pos = text.lower().find(value.lower())
        if pos == -1:
            return text[:200]

        start = max(0, pos - window)
        end = min(len(text), pos + len(value) + window)

        return "..." + text[start:end] + "..."

    def _fuzzy_match(self, value: str, text: str) -> str | None:
        """Fuzzy match for numeric/temporal values."""
        # Simple implementation - can be enhanced with regex
        words = value.split()
        for word in words:
            if word in text:
                return self._extract_context(text, word)
        return None

    def _semantic_verify(
        self, claim: Claim, evidence: list[Evidence]
    ) -> tuple[bool, float]:
        """Use LLM for semantic verification.

        Args:
            claim: Claim to verify
            evidence: List of evidence

        Returns:
            (verified, confidence) tuple
        """
        if not evidence:
            return False, 0.0

        # Build verification prompt
        evidence_text = "\n\n".join(
            [f"[{e.source_type}:{e.source_id}] {e.content}" for e in evidence]
        )

        prompt = f"""Verify if the following claim is supported by the evidence.

Claim: {claim.value}
Type: {claim.type}

Evidence:
{evidence_text}

Is the claim verified by the evidence? Respond with JSON:
{{"verified": true/false, "confidence": 0.0-1.0, "reasoning": "..."}}"""

        try:
            response = self.llm_client.generate(
                prompt=prompt,
                model="gpt-4o-mini",
                temperature=0.0,
                max_tokens=200,
            )

            result = json.loads(response)
            return result["verified"], result["confidence"]

        except Exception as e:
            logger.error(f"Semantic verification failed: {e}")
            # Fall back to simple heuristic
            avg_confidence = sum(e.confidence for e in evidence) / len(evidence)
            return avg_confidence > 0.7, avg_confidence
