"""Streaming verification — sentence-level grounding checks during output.

Hybrid approach (ref: trust-and-safety.md §2 "Streaming Verification"):
  - Tokens stream to user immediately (low latency)
  - Background: accumulate at sentence boundaries
  - Per-sentence check against context snapshot
  - If contradiction detected: yield inline warning
  - Post-completion: full response-level verification for audit record
"""

from __future__ import annotations

import re
from dataclasses import dataclass, field
from typing import Any

from core.logging_config import get_logger

logger = get_logger(__name__)

# Sentence boundary: period/question/exclamation followed by space or end
_SENTENCE_RE = re.compile(r"(?<=[.!?])\s+|(?<=[.!?])$")


@dataclass
class StreamingVerifier:
    """Accumulates streamed text and checks sentences against context.

    Usage::

        sv = StreamingVerifier(firewall, context_capture_id)
        for chunk in stream:
            warning = sv.check(chunk)
            yield chunk
            if warning:
                yield warning  # inline ⚠️
        # post-stream: sv.full_text has the accumulated text
    """

    firewall: Any
    context_capture_id: str
    full_text: str = ""
    _buffer: str = ""
    _warned_sentences: int = 0

    def check(self, chunk: str) -> str | None:
        """Accumulate chunk, verify completed sentences.

        Returns warning string if a sentence fails verification, else None.
        """
        self.full_text += chunk
        self._buffer += chunk

        # Split on sentence boundaries
        parts = _SENTENCE_RE.split(self._buffer)
        if len(parts) < 2:
            return None  # no complete sentence yet

        # Keep the last (possibly incomplete) part in buffer
        complete = parts[:-1]
        self._buffer = parts[-1]

        # Verify each complete sentence
        for sentence in complete:
            sentence = sentence.strip()
            if len(sentence) < 10:
                continue  # skip trivial fragments
            warning = self._check_sentence(sentence)
            if warning:
                return warning
        return None

    def _check_sentence(self, sentence: str) -> str | None:
        """Verify a single sentence. Returns warning or None."""
        try:
            result = self.firewall.verify_response(
                sentence, self.context_capture_id, mode="warn",
            )
            if result.claims_failed > 0 and result.confidence_score < 0.5:
                self._warned_sentences += 1
                return (
                    f" ⚠️[{result.claims_failed} unverified claim"
                    f"{'s' if result.claims_failed > 1 else ''}] "
                )
        except Exception as e:
            logger.debug("Streaming verify skipped: %s", e)
        return None

    def flush(self) -> str | None:
        """Check any remaining buffered text."""
        if self._buffer.strip() and len(self._buffer.strip()) >= 10:
            warning = self._check_sentence(self._buffer.strip())
            self._buffer = ""
            return warning
        self._buffer = ""
        return None
