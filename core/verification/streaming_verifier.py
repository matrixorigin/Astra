"""Streaming verification — sentence-level grounding checks during output.

Ref: trust-and-safety.md §2 "Streaming Verification (Roadmap)"

Hybrid approach:
  - Tokens stream to user immediately (low latency)
  - At sentence boundaries: batch LLM entailment check against context
  - If contradiction detected: yield inline warning
  - Post-completion: full response-level verification for audit record

Batch mode: accumulates sentences and verifies in batches (default 3)
to reduce LLM roundtrips. Falls back to firewall when LLM unavailable.
"""

from __future__ import annotations

import re
from dataclasses import dataclass, field
from typing import Any

from core.logging_config import get_logger

logger = get_logger(__name__)

# Sentence boundary: period/question/exclamation followed by space or end
_SENTENCE_RE = re.compile(r"(?<=[.!?])\s+|(?<=[.!?])$")

_BATCH_ENTAILMENT_PROMPT = """\
Given the following context, determine if each numbered claim is supported.

Context (what the agent was given):
{context}

Claims to verify:
{claims}

For each claim, respond with its number and exactly one verdict.
Format: one line per claim, e.g. "1: SUPPORTED"
Verdicts: SUPPORTED, CONTRADICTED, or UNVERIFIABLE"""

# Keep single-sentence prompt for batch_size=1 or flush with 1 sentence
_ENTAILMENT_PROMPT = """\
Given the following context, determine if the claim is supported.

Context (what the agent was given):
{context}

Claim to verify:
{sentence}

Is this claim SUPPORTED, CONTRADICTED, or UNVERIFIABLE by the context?
Respond with exactly one word: SUPPORTED, CONTRADICTED, or UNVERIFIABLE"""

DEFAULT_BATCH_SIZE = 3
MAX_CONTEXT_CHARS = 3000


@dataclass
class StreamingVerifier:
    """Accumulates streamed text and checks sentences against context.

    Batches sentences (default 3) to reduce LLM calls. A 10-sentence
    response uses ~3-4 LLM calls instead of 10.

    Usage::

        sv = StreamingVerifier(firewall, context_capture_id, llm_client=llm)
        for chunk in stream:
            warnings = sv.check(chunk)
            yield chunk
            for w in warnings:
                yield w
        # flush remaining
        for w in sv.flush():
            yield w
    """

    firewall: Any
    context_capture_id: str
    llm_client: Any = None
    batch_size: int = DEFAULT_BATCH_SIZE
    full_text: str = ""
    _buffer: str = ""
    _pending_sentences: list[str] = field(default_factory=list)
    _warned_sentences: int = 0
    _context_text: str | None = None

    def check(self, chunk: str) -> list[str]:
        """Accumulate chunk, verify completed sentences in batches.

        Returns list of warning strings (empty if no issues detected).
        Warnings are emitted when a batch is full and verified.
        """
        self.full_text += chunk
        self._buffer += chunk

        parts = _SENTENCE_RE.split(self._buffer)
        if len(parts) < 2:
            return []

        complete = parts[:-1]
        self._buffer = parts[-1]

        for sentence in complete:
            sentence = sentence.strip()
            if len(sentence) < 10:
                continue
            self._pending_sentences.append(sentence)

        if len(self._pending_sentences) >= self.batch_size:
            return self._verify_batch(self._pending_sentences)
        return []

    def flush(self) -> list[str]:
        """Verify any remaining buffered text and pending sentences."""
        # Add remaining buffer as a sentence if substantial
        remaining = self._buffer.strip()
        if remaining and len(remaining) >= 10:
            self._pending_sentences.append(remaining)
        self._buffer = ""

        if not self._pending_sentences:
            return []
        return self._verify_batch(self._pending_sentences)

    def _verify_batch(self, sentences: list[str]) -> list[str]:
        """Verify a batch of sentences. Drains the pending list."""
        batch = list(sentences)
        self._pending_sentences.clear()

        if self.llm_client:
            return self._llm_batch_check(batch)
        return self._firewall_batch_check(batch)

    def _llm_batch_check(self, sentences: list[str]) -> list[str]:
        """Verify multiple sentences in a single LLM call."""
        try:
            if self._context_text is None:
                self._context_text = self._load_context()
            if not self._context_text:
                return []

            from core.llm.models import LLMMessage

            context = self._context_text[:MAX_CONTEXT_CHARS]

            # Single sentence: use simpler prompt
            if len(sentences) == 1:
                return self._llm_single_check(sentences[0], context)

            claims = "\n".join(
                f"{i + 1}. {s}" for i, s in enumerate(sentences)
            )
            prompt = _BATCH_ENTAILMENT_PROMPT.format(
                context=context, claims=claims,
            )
            response = self.llm_client.chat(
                messages=[LLMMessage(role="user", content=prompt)],
                user_id="system",
                session_id="stream_verify",
            )
            return self._parse_batch_verdicts(
                response.content or "", sentences,
            )
        except Exception as e:
            logger.debug("LLM batch entailment check skipped: %s", e)
            return []

    def _llm_single_check(
        self, sentence: str, context: str,
    ) -> list[str]:
        """Verify a single sentence with the simpler prompt."""
        try:
            from core.llm.models import LLMMessage

            prompt = _ENTAILMENT_PROMPT.format(
                context=context, sentence=sentence,
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
                    "Streaming verify: CONTRADICTED sentence: %s",
                    sentence[:80],
                )
                return [" ⚠️[contradicts context] "]
        except Exception as e:
            logger.debug("LLM single entailment check skipped: %s", e)
        return []

    def _parse_batch_verdicts(
        self, response_text: str, sentences: list[str],
    ) -> list[str]:
        """Parse batch LLM response, return warnings for contradictions."""
        warnings: list[str] = []
        lines = response_text.strip().splitlines()

        for line in lines:
            line = line.strip().upper()
            if "CONTRADICTED" not in line:
                continue
            # Extract claim number from "N: CONTRADICTED" format
            idx = self._extract_claim_index(line, len(sentences))
            if idx is not None:
                self._warned_sentences += 1
                logger.warning(
                    "Streaming verify: CONTRADICTED sentence: %s",
                    sentences[idx][:80],
                )
                warnings.append(" ⚠️[contradicts context] ")
        return warnings

    @staticmethod
    def _extract_claim_index(line: str, num_claims: int) -> int | None:
        """Extract 0-based claim index from a verdict line like '2: CONTRADICTED'."""
        for token in line.split():
            cleaned = token.strip(":.)#")
            if cleaned.isdigit():
                idx = int(cleaned) - 1  # 1-based → 0-based
                if 0 <= idx < num_claims:
                    return idx
        return None

    def _firewall_batch_check(self, sentences: list[str]) -> list[str]:
        """Fallback: verify each sentence via firewall."""
        warnings: list[str] = []
        for sentence in sentences:
            try:
                result = self.firewall.verify_response(
                    sentence, self.context_capture_id, mode="warn",
                )
                if result.claims_failed > 0 and result.confidence_score < 0.5:
                    self._warned_sentences += 1
                    warnings.append(
                        f" ⚠️[{result.claims_failed} unverified claim"
                        f"{'s' if result.claims_failed > 1 else ''}] "
                    )
            except Exception as e:
                logger.debug("Streaming firewall check skipped: %s", e)
        return warnings

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
