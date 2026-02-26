"""MemoryRetriever — MO-native hybrid retrieval for the memories table.

Single SQL combining L2_DISTANCE + MATCH AGAINST + temporal decay + confidence.
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

# SQL template — single query, all scoring in MO engine
_HYBRID_SQL = text("""\
SELECT m.memory_id, m.content, m.memory_type, m.confidence, m.observed_at,
    (
        :w_vec  * (1.0 / (1.0 + L2_DISTANCE(m.embedding, :query_vec))) +
        :w_kw   * CASE WHEN MATCH(m.content) AGAINST(:query_text IN BOOLEAN MODE)
                       THEN 1.0 ELSE 0.0 END +
        :w_time * EXP(-TIMESTAMPDIFF(HOUR, m.observed_at, NOW()) / :decay_hours) +
        :w_conf * (m.confidence * EXP(-TIMESTAMPDIFF(DAY, m.observed_at, NOW()) / :half_life))
    ) AS relevance
FROM memories m
WHERE m.user_id = :uid AND m.is_active = 1
    AND m.memory_type IN :types
ORDER BY relevance DESC
LIMIT :lim
""")

# Fallback: no embedding available
_FALLBACK_SQL = text("""\
SELECT m.memory_id, m.content, m.memory_type, m.confidence, m.observed_at,
    (
        :w_kw   * CASE WHEN MATCH(m.content) AGAINST(:query_text IN BOOLEAN MODE)
                       THEN 1.0 ELSE 0.0 END +
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
        """Retrieve memories ranked by hybrid relevance.

        Falls back to keyword + confidence + temporal if no embedding provided.
        """
        if weights is None:
            weights = TASK_WEIGHTS.get(task_hint or "default", TASK_WEIGHTS["default"])

        if memory_types is None:
            memory_types = [MemoryType.PROFILE, MemoryType.EPISODIC, MemoryType.SEMANTIC, MemoryType.PROCEDURAL]

        type_values = tuple(t.value for t in memory_types)

        with self._db() as db:
            if query_embedding is not None:
                vec_str = "[" + ",".join(str(v) for v in query_embedding) + "]"
                rows = db.execute(
                    _HYBRID_SQL,
                    {
                        "uid": user_id,
                        "query_vec": vec_str,
                        "query_text": query_text,
                        "types": type_values,
                        "w_vec": weights.vector,
                        "w_kw": weights.keyword,
                        "w_time": weights.temporal,
                        "w_conf": weights.confidence,
                        "decay_hours": self.decay_hours,
                        "half_life": self.half_life_days,
                        "lim": limit,
                    },
                ).fetchall()
            else:
                # Redistribute vector weight to other dimensions
                total = weights.keyword + weights.temporal + weights.confidence
                if total > 0:
                    scale = 1.0 / total
                else:
                    scale = 1.0
                rows = db.execute(
                    _FALLBACK_SQL,
                    {
                        "uid": user_id,
                        "query_text": query_text,
                        "types": type_values,
                        "w_kw": weights.keyword * scale,
                        "w_time": weights.temporal * scale,
                        "w_conf": weights.confidence * scale,
                        "decay_hours": self.decay_hours,
                        "half_life": self.half_life_days,
                        "lim": limit,
                    },
                ).fetchall()

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
