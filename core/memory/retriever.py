"""MemoryRetriever — MO-native hybrid retrieval for the memories table.

Scoring strategy (3 phases):
  Phase 1: SQL-side — keyword filter (MATCH in WHERE) + temporal/confidence scoring
  Phase 2: SQL-side — vector candidates via L2_DISTANCE (when embedding provided)
  Phase 3: App-side — merge + re-rank using all 4 dimensions (vector, keyword, temporal, confidence)

MO Fulltext Limitation: MATCH() AGAINST() can only be used in WHERE clause
for filtering, not in SELECT for arithmetic scoring. So keyword presence is
a binary signal (1.0 if matched, 0.0 if not) rather than a continuous score.
"""

from __future__ import annotations

import logging
import math
import time
from typing import Optional

from sqlalchemy import text

from core.db_consumer import DbConsumer, DbFactory
from core.memory.metrics import metrics, Timer
from core.memory.types import Memory, MemoryType, RetrievalWeights

logger = logging.getLogger(__name__)

# Task-hint → weight presets (all sum to 1.0)
TASK_WEIGHTS: dict[str, RetrievalWeights] = {
    "code": RetrievalWeights(vector=0.3, keyword=0.25, temporal=0.15, confidence=0.3),
    "reasoning": RetrievalWeights(vector=0.4, keyword=0.1, temporal=0.2, confidence=0.3),
    "recall": RetrievalWeights(vector=0.5, keyword=0.1, temporal=0.3, confidence=0.1),
    "default": RetrievalWeights(vector=0.3, keyword=0.2, temporal=0.2, confidence=0.3),
}

# Phase 1: keyword filter + temporal/confidence scoring in SQL
_KEYWORD_SQL = """\
SELECT m.memory_id, m.content, m.memory_type, m.confidence, m.observed_at, m.session_id,
    (
        :w_time * EXP(-TIMESTAMPDIFF(HOUR, m.observed_at, NOW()) / :decay_hours) +
        :w_conf * (m.confidence * EXP(-TIMESTAMPDIFF(DAY, m.observed_at, NOW()) / :half_life))
    ) AS relevance
FROM memories m
WHERE m.user_id = :uid AND m.is_active = 1
    AND m.memory_type IN :types
    AND MATCH(m.content) AGAINST(:query_text IN BOOLEAN MODE)
    {session_filter}
ORDER BY relevance DESC
LIMIT :lim
"""

# Phase 1 fallback: no keyword match, temporal/confidence only
_FALLBACK_SQL = """\
SELECT m.memory_id, m.content, m.memory_type, m.confidence, m.observed_at, m.session_id,
    (
        :w_time * EXP(-TIMESTAMPDIFF(HOUR, m.observed_at, NOW()) / :decay_hours) +
        :w_conf * (m.confidence * EXP(-TIMESTAMPDIFF(DAY, m.observed_at, NOW()) / :half_life))
    ) AS relevance
FROM memories m
WHERE m.user_id = :uid AND m.is_active = 1
    AND m.memory_type IN :types
    {session_filter}
ORDER BY relevance DESC
LIMIT :lim
"""

# Phase 2: vector candidates via L2_DISTANCE (over-fetch for merge)
_VECTOR_SQL = """\
SELECT m.memory_id, m.content, m.memory_type, m.confidence, m.observed_at, m.session_id,
    L2_DISTANCE(m.embedding, :query_vec) AS l2_dist
FROM memories m
WHERE m.user_id = :uid AND m.is_active = 1
    AND m.memory_type IN :types
    AND m.embedding IS NOT NULL
    {session_filter}
ORDER BY l2_dist ASC
LIMIT :lim
"""


def _safe_exp(x: float) -> float:
    """exp() clamped to avoid overflow."""
    return math.exp(max(-500.0, min(500.0, x)))


class MemoryRetriever(DbConsumer):
    """Query-aware hybrid retrieval over the memories table.

    When query_embedding is provided, runs true 4-dimensional hybrid scoring:
    vector similarity, keyword match, temporal recency, confidence decay.
    When no embedding, falls back to keyword + temporal + confidence (3-dim).
    """

    def __init__(self, db_factory: DbFactory, decay_hours: float = 720.0, half_life_days: float = 30.0):
        super().__init__(db_factory)
        self.decay_hours = decay_hours
        self.half_life_days = half_life_days

    def retrieve(
        self,
        user_id: str,
        query_text: str,
        session_id: str,
        query_embedding: Optional[list[float]] = None,
        memory_types: Optional[list[MemoryType]] = None,
        limit: int = 10,
        task_hint: Optional[str] = None,
        weights: Optional[RetrievalWeights] = None,
        include_cross_session: bool = True,
    ) -> list[Memory]:
        """Retrieve memories ranked by multi-dimensional relevance.

        When query_embedding is provided:
          Phase 1: keyword/fallback SQL → candidates with temporal+confidence scores
          Phase 2: vector SQL → candidates with L2 distance
          Phase 3: merge both sets, re-rank by weighted 4-dim score, return top-K

        When no embedding:
          Phase 1 only, weights redistributed across temporal+confidence.
        """
        if weights is None:
            weights = TASK_WEIGHTS.get(task_hint or "default", TASK_WEIGHTS["default"])

        if memory_types is None:
            memory_types = [MemoryType.PROFILE, MemoryType.EPISODIC, MemoryType.SEMANTIC, MemoryType.PROCEDURAL]

        type_values = tuple(t.value for t in memory_types)

        if include_cross_session:
            session_filter = "AND (m.session_id = :session_id OR m.session_id IS NULL)"
        else:
            session_filter = "AND m.session_id = :session_id"

        base_params = {
            "uid": user_id,
            "types": type_values,
            "decay_hours": self.decay_hours,
            "half_life": self.half_life_days,
            "session_id": session_id,
        }

        with Timer("retriever_retrieve"):
            # Phase 1: keyword/fallback candidates (over-fetch for merge headroom)
            phase1_limit = limit * 2 if query_embedding else limit
            phase1 = self._phase1_keyword_or_fallback(
                query_text, session_filter, base_params, weights, phase1_limit,
            )

            # No embedding → redistribute weights to temporal+confidence, return phase 1
            if not query_embedding:
                return [self._to_memory(c, user_id) for c in phase1[:limit]]

            # Phase 2: vector candidates
            phase2 = self._phase2_vector(
                query_embedding, session_filter, base_params, type_values, limit * 2,
            )

            # Phase 3: merge + re-rank with full 4-dim scoring
            return self._merge_and_rerank(
                phase1, phase2, user_id, weights, limit,
            )

    def _phase1_keyword_or_fallback(
        self,
        query_text: str,
        session_filter: str,
        base_params: dict,
        weights: RetrievalWeights,
        limit: int,
    ) -> list[_CandidateRow]:
        """Phase 1: keyword filter or fallback, scored by temporal + confidence."""
        # Normalize weights for the 2 SQL-side dimensions
        total = weights.temporal + weights.confidence
        w_time = weights.temporal / total if total > 0 else 0.5
        w_conf = weights.confidence / total if total > 0 else 0.5

        params = {**base_params, "w_time": w_time, "w_conf": w_conf, "lim": limit}

        with self._db() as db:
            if query_text and query_text.strip():
                params["query_text"] = query_text
                try:
                    sql = text(_KEYWORD_SQL.format(session_filter=session_filter))
                    rows = db.execute(sql, params).fetchall()
                    if rows:
                        metrics.increment("retrieval_keyword_hits")
                        return [
                            _CandidateRow(r.memory_id, r.content, r.memory_type,
                                          r.confidence, r.observed_at, r.session_id,
                                          keyword_matched=True)
                            for r in rows
                        ]
                except Exception as e:
                    logger.debug("Keyword search failed, falling back: %s", e)
                    metrics.increment("retrieval_keyword_errors")

            sql = text(_FALLBACK_SQL.format(session_filter=session_filter))
            rows = db.execute(sql, params).fetchall()
            metrics.increment("retrieval_fallback_hits")
            return [
                _CandidateRow(r.memory_id, r.content, r.memory_type,
                              r.confidence, r.observed_at, r.session_id,
                              keyword_matched=False)
                for r in rows
            ]

    def _phase2_vector(
        self,
        query_embedding: list[float],
        session_filter: str,
        base_params: dict,
        type_values: tuple,
        limit: int,
    ) -> list[_VectorRow]:
        """Phase 2: vector nearest-neighbor candidates via L2_DISTANCE."""
        vec_str = "[" + ",".join(str(v) for v in query_embedding) + "]"
        params = {**base_params, "query_vec": vec_str, "lim": limit}

        with self._db() as db:
            try:
                sql = text(_VECTOR_SQL.format(session_filter=session_filter))
                rows = db.execute(sql, params).fetchall()
                metrics.increment("retrieval_vector_hits")
                return [
                    _VectorRow(r.memory_id, r.content, r.memory_type,
                               r.confidence, r.observed_at, r.session_id,
                               l2_dist=float(r.l2_dist))
                    for r in rows
                ]
            except Exception as e:
                logger.warning("Vector search failed: %s", e)
                metrics.increment("retrieval_vector_errors")
                return []

    def _merge_and_rerank(
        self,
        phase1: list[_CandidateRow],
        phase2: list[_VectorRow],
        user_id: str,
        weights: RetrievalWeights,
        limit: int,
    ) -> list[Memory]:
        """Phase 3: merge phase1+phase2 candidates, score all 4 dimensions, return top-K."""
        merged: dict[str, _MergedCandidate] = {}

        for c in phase1:
            merged[c.memory_id] = _MergedCandidate(
                memory_id=c.memory_id, content=c.content, memory_type=c.memory_type,
                confidence=c.confidence, observed_at=c.observed_at, session_id=c.session_id,
                keyword_matched=c.keyword_matched, l2_dist=None,
            )

        for v in phase2:
            if v.memory_id in merged:
                merged[v.memory_id].l2_dist = v.l2_dist
            else:
                merged[v.memory_id] = _MergedCandidate(
                    memory_id=v.memory_id, content=v.content, memory_type=v.memory_type,
                    confidence=v.confidence, observed_at=v.observed_at, session_id=v.session_id,
                    keyword_matched=False, l2_dist=v.l2_dist,
                )

        if not merged:
            return []

        now_ts = time.time()
        scored: list[tuple[float, _MergedCandidate]] = []

        for c in merged.values():
            # Vector: 1 / (1 + l2_dist) — 1.0 for identical, decays toward 0
            vec_score = 1.0 / (1.0 + c.l2_dist) if c.l2_dist is not None else 0.0

            # Keyword: binary (MO limitation — MATCH only usable in WHERE)
            kw_score = 1.0 if c.keyword_matched else 0.0

            # Temporal recency: exponential decay
            if c.observed_at:
                age_hours = (now_ts - c.observed_at.timestamp()) / 3600.0
                time_score = _safe_exp(-age_hours / self.decay_hours)
            else:
                time_score = 0.0

            # Confidence with age-based decay
            if c.observed_at:
                age_days = (now_ts - c.observed_at.timestamp()) / 86400.0
                conf_score = c.confidence * _safe_exp(-age_days / self.half_life_days)
            else:
                conf_score = c.confidence

            final = (
                weights.vector * vec_score
                + weights.keyword * kw_score
                + weights.temporal * time_score
                + weights.confidence * conf_score
            )
            scored.append((final, c))

        scored.sort(key=lambda x: x[0], reverse=True)

        return [
            Memory(
                memory_id=c.memory_id,
                user_id=user_id,
                memory_type=MemoryType(c.memory_type),
                content=c.content,
                confidence=c.confidence,
                session_id=c.session_id,
                observed_at=c.observed_at,
            )
            for _, c in scored[:limit]
        ]

    @staticmethod
    def _to_memory(c: _CandidateRow, user_id: str) -> Memory:
        return Memory(
            memory_id=c.memory_id,
            user_id=user_id,
            memory_type=MemoryType(c.memory_type),
            content=c.content,
            confidence=c.confidence,
            session_id=c.session_id,
            observed_at=c.observed_at,
        )


# --- Internal data carriers (not exported) ---

class _CandidateRow:
    __slots__ = ("memory_id", "content", "memory_type", "confidence", "observed_at", "session_id", "keyword_matched")

    def __init__(self, memory_id, content, memory_type, confidence, observed_at, session_id, keyword_matched):
        self.memory_id = memory_id
        self.content = content
        self.memory_type = memory_type
        self.confidence = confidence
        self.observed_at = observed_at
        self.session_id = session_id
        self.keyword_matched = keyword_matched


class _VectorRow:
    __slots__ = ("memory_id", "content", "memory_type", "confidence", "observed_at", "session_id", "l2_dist")

    def __init__(self, memory_id, content, memory_type, confidence, observed_at, session_id, l2_dist):
        self.memory_id = memory_id
        self.content = content
        self.memory_type = memory_type
        self.confidence = confidence
        self.observed_at = observed_at
        self.session_id = session_id
        self.l2_dist = l2_dist


class _MergedCandidate:
    __slots__ = ("memory_id", "content", "memory_type", "confidence", "observed_at", "session_id", "keyword_matched", "l2_dist")

    def __init__(self, memory_id, content, memory_type, confidence, observed_at, session_id, keyword_matched, l2_dist):
        self.memory_id = memory_id
        self.content = content
        self.memory_type = memory_type
        self.confidence = confidence
        self.observed_at = observed_at
        self.session_id = session_id
        self.keyword_matched = keyword_matched
        self.l2_dist = l2_dist
