"""Streaming verification — sentence-level grounding checks during output.

Ref: trust-and-safety.md §2 "Streaming Verification (Roadmap)"

Hybrid approach:
  - Tokens stream to user immediately (low latency)
  - At sentence boundaries: LLM entailment check against context
  - If contradiction detected: yield inline warning
  - Post-completion: full response-level verification for audit record

Uses LLM-as-judge for entailment when available, falls back to
firewall.verify_response when not.
"""

from __future__ import annotations

import re
from dataclasses import dataclass, field
from typing import Any

from core.logging_config import get_logger

logger = get_logger(__name__)

# Sentence boundary: period/question/exclamation followed by space or end
_SENTENCE_RE = re.compile(r"(?<=[.!?])\s+|(?<=[.!?])$")

_ENTAILMENT_PROMPT = """\
Given the following context, determine if the claim is supported.

Context (what the agent was given):
{context}

Claim to verify:
{sentence}

Is this claim SUPPORTED, CONTRADICTED, or UNVERIFIABLE by the context?
Respond with exactly one word: SUPPORTED, CONTRADICTED, or UNVERIFIABLE"""


@dataclass
class StreamingVerifier:
    """Accumulates streamed text and checks sentences against context.

    Usage::

        sv = StreamingVerifier(firewall, context_capture_id, llm_client=llm)
        for chunk in stream:
            warning = sv.check(chunk)
            yield chunk
            if warning:
                yield warning  # inline ⚠️
        # post-stream: sv.full_text has the accumulated text
    """

    firewall: Any
    context_capture_id: str
    llm_client: Any = None
    full_text: str = ""
    _buffer: str = ""
    _warned_sentences: int = 0
    _context_text: str | None = None

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
        """Verify a single sentence via LLM entailment or firewall fallback."""
        # Try LLM entailment first (design target)
        if self.llm_client:
            return self._llm_entailment_check(sentence)
        # Fallback to firewall
        return self._firewall_check(sentence)

    def _llm_entailment_check(self, sentence: str) -> str | None:
        """Use LLM to judge if sentence is supported by context."""
        try:
            # Lazy-load context text once
            if self._context_text is None:
                self._context_text = self._load_context()

            if not self._context_text:
                return None  # no context to check against

            from core.llm.base import LLMMessage
            prompt = _ENTAILMENT_PROMPT.format(
                context=self._context_text[:3000],
                sentence=sentence,
            )
            response = self.llm_client.chat(
                messages=[LLMMessage(role="user", content=prompt)],
                user_id="system",
                session_id="stream_verify",
            )
            verdict = (response.content or "").strip().upper()
            if "CONTRADICTED" in verdict:
                self._warned_sentences += 1
                logger.warning(
                    "Streaming verify: CONTRADICTED sentence: %s", sentence[:80],
                )
                return " ⚠️[contradicts context] "
        except Exception as e:
            logger.debug("LLM entailment check skipped: %s", e)
        return None

    def _firewall_check(self, sentence: str) -> str | None:
        """Fallback: use firewall for sentence verification."""
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
            logger.debug("Streaming firewall check skipped: %s", e)
        return None

    def _load_context(self) -> str:
        """Load context snapshot text for entailment checking."""
        try:
            snapshot = self.firewall.context_manager.load_snapshot(
                self.context_capture_id,
            )
            parts = [getattr(snapshot, "system_prompt", "") or ""]
            for ev in getattr(snapshot, "selected_events", []):
                parts.append(ev.get("content", ""))
            for code in getattr(snapshot, "code_context", []):
                parts.append(code.get("content", ""))
            return "\n".join(p for p in parts if p)
        except Exception as e:
            logger.debug("Context load for streaming verify failed: %s", e)
            return ""

    def flush(self) -> str | None:
        """Check any remaining buffered text."""
        if self._buffer.strip() and len(self._buffer.strip()) >= 10:
            warning = self._check_sentence(self._buffer.strip())
            self._buffer = ""
            return warning
        self._buffer = ""
        return None
