"""LLM-based claim extraction for hallucination firewall.

Uses a small, fast model (gpt-4o-mini) to extract verifiable claims
from LLM responses, catching implicit assertions and causal reasoning
that regex patterns miss.
"""

import json
from dataclasses import dataclass

from core.logging_config import get_logger

logger = get_logger(__name__)


@dataclass
class Claim:
    """A verifiable claim extracted from text."""

    type: str  # 'numeric' | 'temporal' | 'causal' | 'factual'
    value: str
    context: str  # Surrounding text
    position: int  # Character position in original text
    confidence: float = 1.0  # Extraction confidence


class LLMClaimExtractor:
    """Extract verifiable claims using LLM."""

    EXTRACTION_PROMPT = """Extract all verifiable claims from the following text.

A verifiable claim is a statement that can be checked against data:
- Numeric: "5 files changed", "95% accuracy", "3 PRs merged"
- Temporal: "yesterday", "last week", "2026-02-14"
- Causal: "the test fails because X", "this causes Y"
- Factual: "the API returns JSON", "the function is async"

Text:
{text}

Return JSON array of claims:
[
  {{"type": "numeric", "value": "5 files", "context": "...5 files changed...", "position": 42}},
  {{"type": "causal", "value": "test fails because of timeout", "context": "...", "position": 100}}
]

Only extract specific, verifiable claims. Skip general statements."""

    def __init__(self, llm_client):
        """Initialize with LLM client.

        Args:
            llm_client: LLM client with generate() method
        """
        self.llm_client = llm_client

    def extract(self, text: str) -> list[Claim]:
        """Extract claims using LLM.

        Args:
            text: Response text to analyze

        Returns:
            List of extracted claims
        """
        if not text or not text.strip():
            return []

        try:
            prompt = self.EXTRACTION_PROMPT.format(text=text)

            response = self.llm_client.generate(
                prompt=prompt,
                model="gpt-4o-mini",
                temperature=0.0,
                max_tokens=1000,
            )

            # Parse JSON response
            claims_data = json.loads(response)

            claims = []
            for item in claims_data:
                claims.append(
                    Claim(
                        type=item["type"],
                        value=item["value"],
                        context=item.get("context", ""),
                        position=item.get("position", 0),
                        confidence=item.get("confidence", 1.0),
                    )
                )

            logger.info(f"Extracted {len(claims)} claims using LLM")
            return claims

        except json.JSONDecodeError as e:
            logger.error(f"Failed to parse LLM response as JSON: {e}")
            return []
        except Exception as e:
            logger.error(f"LLM claim extraction failed: {e}")
            return []
