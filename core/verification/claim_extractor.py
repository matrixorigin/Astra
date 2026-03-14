"""Claim extraction from LLM responses.

Extracts verifiable claims (numbers, dates, references) from text.
"""

import re
from dataclasses import dataclass


@dataclass
class Claim:
    """A verifiable claim extracted from text."""

    type: str  # 'number' | 'date' | 'reference' | 'boolean'
    value: str
    context: str  # Surrounding text
    position: int  # Character position in original text


class ClaimExtractor:
    """Extract verifiable claims from text."""

    def extract(self, text: str) -> list[Claim]:
        """Extract all verifiable claims from text."""
        claims = []

        # Extract numbers (integers, floats, percentages)
        claims.extend(self._extract_numbers(text))

        # Extract dates (YYYY-MM-DD, relative dates)
        claims.extend(self._extract_dates(text))

        # Extract references (PR #123, issue #456, commit abc123)
        claims.extend(self._extract_references(text))

        # Extract boolean claims (is/are/has/have statements)
        claims.extend(self._extract_booleans(text))

        return claims

    def _extract_numbers(self, text: str) -> list[Claim]:
        """Extract numeric claims."""
        claims = []

        # Match numbers with optional units
        pattern = r"\b(\d+(?:\.\d+)?(?:%|K|M|B)?)\b"

        for match in re.finditer(pattern, text):
            value = match.group(1)
            pos = match.start()
            context = self._get_context(text, pos, window=30)

            claims.append(Claim(type="number", value=value, context=context, position=pos))

        return claims

    def _extract_dates(self, text: str) -> list[Claim]:
        """Extract date claims."""
        claims = []

        # ISO dates (YYYY-MM-DD)
        pattern = r"\b(\d{4}-\d{2}-\d{2})\b"

        for match in re.finditer(pattern, text):
            value = match.group(1)
            pos = match.start()
            context = self._get_context(text, pos, window=30)

            claims.append(Claim(type="date", value=value, context=context, position=pos))

        return claims

    def _extract_references(self, text: str) -> list[Claim]:
        """Extract reference claims (PR #123, issue #456)."""
        claims = []

        # GitHub references
        patterns = [
            r"\bPR\s*#(\d+)\b",
            r"\bissue\s*#(\d+)\b",
            r"\bcommit\s+([a-f0-9]{7,40})\b",
        ]

        for pattern in patterns:
            for match in re.finditer(pattern, text, re.IGNORECASE):
                value = match.group(0)
                pos = match.start()
                context = self._get_context(text, pos, window=30)

                claims.append(Claim(type="reference", value=value, context=context, position=pos))

        return claims

    def _extract_booleans(self, text: str) -> list[Claim]:
        """Extract boolean claims (is/are/has/have statements).

        Only extract specific, verifiable statements, not general prose.
        """
        claims = []

        # More specific pattern: "X is/are/has/have Y" where X and Y are concrete
        # Avoid matching general prose like "This is a general response"
        pattern = r"\b([A-Z][a-z]+(?:\s+[A-Z][a-z]+)*)\s+(is|are|has|have)\s+(\d+|[A-Z][a-z]+)\b"

        for match in re.finditer(pattern, text):
            value = match.group(0)
            pos = match.start()
            context = self._get_context(text, pos, window=50)

            claims.append(Claim(type="boolean", value=value, context=context, position=pos))

        return claims

    def _get_context(self, text: str, position: int, window: int = 30) -> str:
        """Get surrounding context for a claim."""
        start = max(0, position - window)
        end = min(len(text), position + window)
        return text[start:end]
