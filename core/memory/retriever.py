"""MemoryRetriever — MO-native hybrid retrieval for the memories table.

Scoring strategy (3 phases):
  Phase 1: SQL-side — keyword filter (MATCH in WHERE) + temporal/confidence scoring
  Phase 2: SQL-side — vector candidates via L2_DISTANCE (when embedding provided)
  Phase 3: App-side — merge + re-rank using all 4 dimensions (vector, keyword, temporal, confidence)

MO Fulltext Limitation: MATCH() AGAINST() can only be used in WHERE clause
for filtering, not in SELECT for arithmetic scoring. So keyword presence is
a binary signal (1.0 if matched, 0.0 if not) rather than a continuous score.

Supports EXPLAIN ANALYZE mode: pass explain=True to get execution stats.
"""

from __future__ import annotations

import logging
import math
import time
from dataclasses import dataclass
from typing import Optional

from sqlalchemy import text

from core.db_consumer import DbConsumer, DbFactory
from core.memory.explain import RetrievalStats
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


# --- Internal data carriers ---

@dataclass
class _Candidate:
    memory_id: str
    content: str
    memory_type: str
    confidence: float
    observed_at: object
    session_id: Optional[str]
    keyword_matched: bool = False
    l2_dist: Optional[float] = None


@dataclass
class _PhaseStats:
    """Stats from a single phase."""
    keyword_attempted: bool = False
    keyword_hit: bool = False
    keyword_error: Optional[str] = None
    vector_attempted: bool = False
    vector_hit: bool = False
    vector_error: Optional[str] = None


class MemoryRetriever(DbConsumer):
    """Query-aware hybrid retrieval over the memories table.

    When query_embedding is provided, runs true 4-dimensional hybrid scoring:
    vector similarity, keyword match, temporal recency, confidence decay.
    When no embedding, falls back to keyword + temporal + confidence (3-dim).
    
    Supports explain=True for EXPLAIN ANALYZE style execution stats.
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
        explain: bool = False,
    ) -> tuple[list[Memory], Optional[RetrievalStats]]:
        """Retrieve memories ranked by multi-dimensional relevance.
        
        Args:
            explain: If True, return execution stats (like EXPLAIN ANALYZE).
            
        Returns:
            (memories, stats) — stats is None when explain=False.
        """
        start = time.time() if explain else 0
        stats = RetrievalStats() if explain else None

        weights = weights or TASK_WEIGHTS.get(task_hint or "default", TASK_WEIGHTS["default"])
        memory_types = memory_types or [MemoryType.PROFILE, MemoryType.EPISODIC, MemoryType.SEMANTIC, MemoryType.PROCEDURAL]
        type_values = tuple(t.value for t in memory_types)

        session_filter = (
            "AND (m.session_id = :session_id OR m.session_id IS NULL)"
            if include_cross_session else "AND m.session_id = :session_id"
        )
        base_params = {
            "uid": user_id, "types": type_values,
            "decay_hours": self.decay_hours, "half_life": self.half_life_days,
            "session_id": session_id,
        }

        with Timer("retriever_retrieve"):
            # Phase 1
            p1_start = time.time() if explain else 0
            phase1, p1_stats = self._phase1(
                query_text, session_filter, base_params, weights,
                limit * 2 if query_embedding else limit,
            )
            if stats:
                stats.keyword_attempted = p1_stats.keyword_attempted
                stats.keyword_hit = p1_stats.keyword_hit
                stats.keyword_error = p1_stats.keyword_error
                stats.phase1_candidates = len(phase1)
                stats.phase1_ms = (time.time() - p1_start) * 1000

            # No embedding → phase 1 only
            if not query_embedding:
                memories = [self._to_memory(c, user_id) for c in phase1[:limit]]
                if stats:
                    stats.final_count = len(memories)
                    stats.total_ms = (time.time() - start) * 1000
                return memories, stats

            # Phase 2
            p2_start = time.time() if explain else 0
            phase2, p2_stats = self._phase2(query_embedding, session_filter, base_params, limit * 2)
            if stats:
                stats.vector_attempted = p2_stats.vector_attempted
                stats.vector_hit = p2_stats.vector_hit
                stats.vector_error = p2_stats.vector_error
                stats.phase2_candidates = len(phase2)
                stats.phase2_ms = (time.time() - p2_start) * 1000

            # Phase 3: merge
            merge_start = time.time() if explain else 0
            memories = self._merge(phase1, phase2, user_id, weights, limit)
            if stats:
                stats.merged_candidates = len({c.memory_id for c in phase1} | {c.memory_id for c in phase2})
                stats.final_count = len(memories)
                stats.merge_ms = (time.time() - merge_start) * 1000
                stats.total_ms = (time.time() - start) * 1000

            return memories, stats

    def _phase1(
        self, query_text: str, session_filter: str, base_params: dict,
        weights: RetrievalWeights, limit: int,
    ) -> tuple[list[_Candidate], _PhaseStats]:
        """Phase 1: keyword or fallback."""
        total = weights.temporal + weights.confidence
        w_time = weights.temporal / total if total > 0 else 0.5
        w_conf = weights.confidence / total if total > 0 else 0.5
        params = {**base_params, "w_time": w_time, "w_conf": w_conf, "lim": limit}
        stats = _PhaseStats()

        with self._db() as db:
            # Try keyword search
            if query_text and query_text.strip():
                stats.keyword_attempted = True
                params["query_text"] = query_text
                try:
                    rows = db.execute(text(_KEYWORD_SQL.format(session_filter=session_filter)), params).fetchall()
                    if rows:
                        metrics.increment("retrieval_keyword_hits")
                        stats.keyword_hit = True
                        return [_Candidate(r.memory_id, r.content, r.memory_type, r.confidence,
                                           r.observed_at, r.session_id, keyword_matched=True) for r in rows], stats
                except Exception as e:
                    logger.debug("Keyword search failed: %s", e)
                    metrics.increment("retrieval_keyword_errors")
                    stats.keyword_error = str(e)

            # Fallback
            rows = db.execute(text(_FALLBACK_SQL.format(session_filter=session_filter)), params).fetchall()
            metrics.increment("retrieval_fallback_hits")
            return [_Candidate(r.memory_id, r.content, r.memory_type, r.confidence,
                               r.observed_at, r.session_id) for r in rows], stats

    def _phase2(
        self, query_embedding: list[float], session_filter: str, base_params: dict, limit: int,
    ) -> tuple[list[_Candidate], _PhaseStats]:
        """Phase 2: vector search."""
        vec_str = "[" + ",".join(str(v) for v in query_embedding) + "]"
        params = {**base_params, "query_vec": vec_str, "lim": limit}
        stats = _PhaseStats(vector_attempted=True)

        with self._db() as db:
            try:
                rows = db.execute(text(_VECTOR_SQL.format(session_filter=session_filter)), params).fetchall()
                metrics.increment("retrieval_vector_hits")
                stats.vector_hit = bool(rows)
                return [_Candidate(r.memory_id, r.content, r.memory_type, r.confidence,
                                   r.observed_at, r.session_id, l2_dist=float(r.l2_dist)) for r in rows], stats
            except Exception as e:
                logger.warning("Vector search failed: %s", e)
                metrics.increment("retrieval_vector_errors")
                stats.vector_error = str(e)
                return [], stats

    def _merge(
        self, phase1: list[_Candidate], phase2: list[_Candidate],
        user_id: str, weights: RetrievalWeights, limit: int,
    ) -> list[Memory]:
        """Phase 3: merge and re-rank."""
        merged: dict[str, _Candidate] = {}
        for c in phase1:
            merged[c.memory_id] = c
        for c in phase2:
            if c.memory_id in merged:
                merged[c.memory_id].l2_dist = c.l2_dist
            else:
                merged[c.memory_id] = c

        if not merged:
            return []

        now_ts = time.time()
        scored: list[tuple[float, _Candidate]] = []

        for c in merged.values():
            vec_score = 1.0 / (1.0 + c.l2_dist) if c.l2_dist is not None else 0.0
            kw_score = 1.0 if c.keyword_matched else 0.0

            if c.observed_at:
                age_hours = (now_ts - c.observed_at.timestamp()) / 3600.0
                time_score = _safe_exp(-age_hours / self.decay_hours)
                age_days = age_hours / 24.0
                conf_score = c.confidence * _safe_exp(-age_days / self.half_life_days)
            else:
                time_score, conf_score = 0.0, c.confidence

            final = (weights.vector * vec_score + weights.keyword * kw_score +
                     weights.temporal * time_score + weights.confidence * conf_score)
            scored.append((final, c))

        scored.sort(key=lambda x: x[0], reverse=True)
        return [self._to_memory(c, user_id) for _, c in scored[:limit]]

    @staticmethod
    def _to_memory(c: _Candidate, user_id: str) -> Memory:
        return Memory(
            memory_id=c.memory_id, user_id=user_id,
            memory_type=MemoryType(c.memory_type), content=c.content,
            confidence=c.confidence, session_id=c.session_id, observed_at=c.observed_at,
        )
