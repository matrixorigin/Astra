"""LLM response guard: detect prompt leakage and degenerate outputs.

This module provides a centralized defense layer that can be used by ALL LLM
call paths — streaming (chat.py), non-streaming (LLMClient.chat), and the
RunEngine/chat_loop.  Previously, detection lived only in chat.py's streaming
path, leaving 30+ non-streaming call sites unprotected.

Three detection strategies, applied in order (cheapest first):

1. **Structural markers** — short, unambiguous section headings that appear
   in the system prompt (e.g. ``## Core Rules``).  These are too short for
   N-gram fingerprinting but are near-impossible to appear in legitimate
   LLM output.  Extracted automatically from ``PromptAssembler`` constants
   so they stay in sync with prompt changes.

2. **N-gram fingerprints** — sliding-window phrases extracted from the
   current turn's system prompt and tool descriptions.  Catches verbatim
   reproduction of longer prompt fragments.

3. **Repetition loop** — detects degenerate outputs like ``it it it it...``
   that indicate a broken model endpoint.
"""

from __future__ import annotations

import logging
import re
from typing import Any

logger = logging.getLogger(__name__)

# ── Structural markers ────────────────────────────────────────────────
# Section headings that appear in the assembled system prompt.  These are
# short strings (< 30 chars) that N-gram fingerprinting misses, but are
# unambiguous indicators of prompt leakage when found in LLM output.
#
# Sourced from:
#   - PromptAssembler._CORE_RULES          → "## Core Rules"
#   - PromptAssembler._REASONING_PROTOCOL  → "## Reasoning Protocol"
#   - PromptAssembler._build_self_model    → "## Self-Model"
#   - ContextManager                       → "## Conversation History"
#   - PromptAssembler._build_constraints   → rule block headers
_STRUCTURAL_MARKERS: tuple[str, ...] = (
    "## Core Rules",
    "## Reasoning Protocol",
    "## Self-Model",
    "## Conversation History",
    "File editing rules:",
    "Tool selection rules:",
    "Reflection rules:",
    "Introspection rules:",
)

# ── N-gram fingerprint parameters ─────────────────────────────────────
_FINGERPRINT_NGRAM_WORDS = 8
_FINGERPRINT_MIN_LEN = 30

# ── Repetition detection ──────────────────────────────────────────────
_REPEAT_THRESHOLD = 8


# =====================================================================
# Public API
# =====================================================================


def build_fingerprints(
    llm_messages: list[dict[str, Any]],
    tools_schema: list[dict[str, Any]] | None = None,
) -> list[str]:
    """Extract N-gram phrases from system prompt and tool descriptions.

    Returns lowercased phrases of ``_FINGERPRINT_NGRAM_WORDS`` words that are
    at least ``_FINGERPRINT_MIN_LEN`` characters long.
    """
    texts: list[str] = []
    if llm_messages and llm_messages[0].get("role") == "system":
        texts.append(llm_messages[0].get("content") or "")
    for t in tools_schema or []:
        desc = t.get("function", {}).get("description") or ""
        if desc:
            texts.append(desc)

    phrases: list[str] = []
    for text in texts:
        words = text.split()
        for i in range(len(words) - _FINGERPRINT_NGRAM_WORDS + 1):
            phrase = " ".join(words[i : i + _FINGERPRINT_NGRAM_WORDS])
            if len(phrase) >= _FINGERPRINT_MIN_LEN:
                phrases.append(phrase.lower())
    return phrases


def is_prompt_leaked(
    text: str,
    fingerprints: list[str] | None = None,
) -> bool:
    """Return True if *text* contains prompt content.

    Checks structural markers first (O(k) with k ≈ 8), then N-gram
    fingerprints.  A single match is sufficient — legitimate LLM output
    should never reproduce system prompt fragments verbatim.
    """
    if not text:
        return False

    # Strategy 1: structural markers (cheap, catches short headings)
    for marker in _STRUCTURAL_MARKERS:
        if marker in text:
            return True

    # Strategy 2: N-gram fingerprints (catches longer fragments)
    if fingerprints:
        lower = text.lower()
        if any(fp in lower for fp in fingerprints):
            return True

    return False


def is_repetition_loop(text: str) -> bool:
    """Return True if *text* contains a word repeated ≥ threshold times consecutively.

    Catches degenerate outputs like ``it it it it...`` or ``game game game...``
    that indicate a broken model endpoint.
    """
    if not text:
        return False
    words = text.split()
    if len(words) < _REPEAT_THRESHOLD:
        return False
    count = 1
    for i in range(1, len(words)):
        if words[i].lower() == words[i - 1].lower():
            count += 1
            if count >= _REPEAT_THRESHOLD:
                return True
        else:
            count = 1
    return False


def is_degenerate(
    text: str,
    fingerprints: list[str] | None = None,
) -> str | None:
    """Combined check — returns a reason string or None if clean.

    Convenience wrapper that runs all checks and returns the first failure
    reason, or ``None`` if the response is clean.  Useful for callers that
    just need a go/no-go decision.
    """
    if is_prompt_leaked(text, fingerprints):
        return "PROMPT_LEAK"
    if is_repetition_loop(text):
        return "REPETITION_LOOP"
    return None
