"""MemoryRetriever — MO-native hybrid retrieval for the memories table.

MO Fulltext Limitation: MATCH() AGAINST() can only be used in WHERE clause
for filtering, not in SELECT for arithmetic scoring. We use a two-phase approach:
1. Filter by keyword match (if query provided)
2. Score by confidence + temporal decay (in SQL)
3. Optionally re-rank by vector similarity (in application)
"""

from __future__ import annotations

import logging
from typing import Optional

from sqlalchemy import text

from core.db_consumer import DbConsumer, DbFactory
from core.memory.types import Memory, MemoryType, RetrievalWeights

logger = logging.getLogger(__name__)

# Task-hint → weight presets
TASK_WEIGHTS: dict[str, RetrievalWeights] = {
    "code": RetrievalWeights(vector=0.3, keyword=0.25, temporal=0.15, confidence=0.3),
    "reasoning": RetrievalWeights(vector=0.4, keyword=0.1, temporal=0.2, confidence=0.3),
    "recall": RetrievalWeights(vector=0.5, keyword=0.1, temporal=0.3, confidence=0.1),
    "default": RetrievalWeights(vector=0.3, keyword=0.2, temporal=0.2, confidence=0.3),
}

# SQL with keyword filter + confidence/temporal scoring
# MATCH is used in WHERE for filtering, score computed from confidence + temporal
_KEYWORD_SQL = text("""\
SELECT m.memory_id, m.content, m.memory_type, m.confidence, m.observed_at,
    (
        :w_time * EXP(-TIMESTAMPDIFF(HOUR, m.observed_at, NOW()) / :decay_hours) +
        :w_conf * (m.confidence * EXP(-TIMESTAMPDIFF(DAY, m.observed_at, NOW()) / :half_life))
    ) AS relevance
FROM memories m
WHERE m.user_id = :uid AND m.is_active = 1
    AND m.memory_type IN :types
    AND MATCH(m.content) AGAINST(:query_text IN BOOLEAN MODE)
ORDER BY relevance DESC
LIMIT :lim
""")

# Fallback: no keyword match, just confidence + temporal
_FALLBACK_SQL = text("""\
SELECT m.memory_id, m.content, m.memory_type, m.confidence, m.observed_at,
    (
        :w_time * EXP(-TIMESTAMPDIFF(HOUR, m.observed_at, NOW()) / :decay_hours) +
        :w_conf * (m.confidence * EXP(-TIMESTAMPDIFF(DAY, m.observed_at, NOW()) / :half_life))
    ) AS relevance
FROM memories m
WHERE m.user_id = :uid AND m.is_active = 1
    AND m.memory_type IN :types
ORDER BY relevance DESC
LIMIT :lim
""")


class MemoryRetriever(DbConsumer):
    """Query-aware hybrid retrieval over the memories table."""

    def __init__(self, db_factory: DbFactory, decay_hours: float = 720.0, half_life_days: float = 30.0):
        super().__init__(db_factory)
        self.decay_hours = decay_hours
        self.half_life_days = half_life_days

    def retrieve(
        self,
        user_id: str,
        query_text: str,
        query_embedding: Optional[list[float]] = None,
        memory_types: Optional[list[MemoryType]] = None,
        limit: int = 10,
        task_hint: Optional[str] = None,
        weights: Optional[RetrievalWeights] = None,
    ) -> list[Memory]:
        """Retrieve memories ranked by relevance.

        Strategy:
        1. Try keyword filter first (MATCH in WHERE)
        2. Fall back to all memories if no keyword matches
        3. Score by confidence + temporal decay
        """
        if weights is None:
            weights = TASK_WEIGHTS.get(task_hint or "default", TASK_WEIGHTS["default"])

        if memory_types is None:
            memory_types = [MemoryType.PROFILE, MemoryType.EPISODIC, MemoryType.SEMANTIC, MemoryType.PROCEDURAL]

        type_values = tuple(t.value for t in memory_types)

        # Normalize weights for temporal + confidence only
        total = weights.temporal + weights.confidence
        if total > 0:
            w_time = weights.temporal / total
            w_conf = weights.confidence / total
        else:
            w_time = 0.5
            w_conf = 0.5

        params = {
            "uid": user_id,
            "types": type_values,
            "w_time": w_time,
            "w_conf": w_conf,
            "decay_hours": self.decay_hours,
            "half_life": self.half_life_days,
            "lim": limit,
        }

        with self._db() as db:
            # Try keyword search first
            if query_text and query_text.strip():
                params["query_text"] = query_text
                try:
                    rows = db.execute(_KEYWORD_SQL, params).fetchall()
                    if rows:
                        return self._to_memories(rows, user_id)
                except Exception as e:
                    logger.debug("Keyword search failed, falling back: %s", e)

            # Fallback: no keyword filter
            rows = db.execute(_FALLBACK_SQL, params).fetchall()
            return self._to_memories(rows, user_id)

    def _to_memories(self, rows, user_id: str) -> list[Memory]:
        return [
            Memory(
                memory_id=r.memory_id,
                user_id=user_id,
                memory_type=MemoryType(r.memory_type),
                content=r.content,
                confidence=r.confidence,
                observed_at=r.observed_at,
            )
            for r in rows
        ]
