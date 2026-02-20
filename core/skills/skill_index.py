"""Semantic skill index for retrieval-based tool selection.

Builds an in-memory embedding index over skill metadata (name + description +
triggers).  At query time, encodes the user query and returns the top-k most
similar skills by cosine similarity — replacing keyword matching as the
primary retrieval path (RAG-MCP pattern, arXiv:2505.03275).

Falls back to empty results if no embeddings are available; callers should
chain with keyword-based SkillSelector as a fallback.
"""

from __future__ import annotations

import math
from dataclasses import dataclass, field
from typing import Any

from core.logging_config import get_logger

logger = get_logger(__name__)


@dataclass
class _Entry:
    name: str
    vector: list[float]


def _cosine(a: list[float], b: list[float]) -> float:
    dot = sum(x * y for x, y in zip(a, b))
    na = math.sqrt(sum(x * x for x in a))
    nb = math.sqrt(sum(x * x for x in b))
    if na == 0 or nb == 0:
        return 0.0
    return dot / (na * nb)


def _skill_text(skill: Any) -> str:
    """Build the text blob that represents a skill for embedding."""
    parts = [skill.name]
    if skill.description:
        parts.append(skill.description)
    if skill.triggers:
        parts.append(" ".join(skill.triggers))
    return " | ".join(parts)


class SkillIndex:
    """In-memory cosine-similarity index over skill embeddings."""

    def __init__(self, embed_fn=None):
        """
        Args:
            embed_fn: callable(str) -> list[float].  If None the index is
                      inert and query() always returns [].
        """
        self._embed = embed_fn
        self._entries: list[_Entry] = []

    # ------------------------------------------------------------------
    # Build
    # ------------------------------------------------------------------

    def build(self, skills: list[Any]) -> int:
        """(Re)build index from a list of SkillMetadata-like objects.

        Returns number of skills indexed.
        """
        if not self._embed:
            return 0
        self._entries.clear()
        for skill in skills:
            text = _skill_text(skill)
            try:
                vec = self._embed(text)
                self._entries.append(_Entry(name=skill.name, vector=vec))
            except Exception as e:  # noqa: BLE001
                logger.warning("Failed to embed skill %s: %s", skill.name, e)
        logger.info("SkillIndex built: %d skills indexed", len(self._entries))
        return len(self._entries)

    # ------------------------------------------------------------------
    # Query
    # ------------------------------------------------------------------

    def query(self, text: str, top_k: int = 10) -> list[str]:
        """Return top-k skill names by cosine similarity to *text*.

        Returns empty list if index is empty or embed_fn is None.
        """
        if not self._entries or not self._embed:
            return []
        try:
            q_vec = self._embed(text)
        except Exception as e:  # noqa: BLE001
            logger.warning("Query embedding failed: %s", e)
            return []

        scored = [(e.name, _cosine(q_vec, e.vector)) for e in self._entries]
        scored.sort(key=lambda x: x[1], reverse=True)
        return [name for name, _ in scored[:top_k]]
