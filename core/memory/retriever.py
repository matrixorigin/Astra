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
from sqlalchemy.sql import func

from core.db_consumer import DbConsumer, DbFactory
from core.memory.explain import RetrievalStats
from core.memory.metrics import MemoryMetrics, Timer
from core.memory.types import TRUST_TIER_HALF_LIVES, Memory, MemoryType, RetrievalWeights, TrustTier

logger = logging.getLogger(__name__)

# Task-hint → weight presets (all sum to 1.0)
TASK_WEIGHTS: dict[str, RetrievalWeights] = {
    "code": RetrievalWeights(vector=0.3, keyword=0.25, temporal=0.15, confidence=0.3),
    "reasoning": RetrievalWeights(vector=0.4, keyword=0.1, temporal=0.2, confidence=0.3),
    "recall": RetrievalWeights(vector=0.5, keyword=0.1, temporal=0.3, confidence=0.1),
    "default": RetrievalWeights(vector=0.3, keyword=0.2, temporal=0.2, confidence=0.3),
}


def _relevance_expr(w_time: float, w_conf: float, decay_hours: float, half_life: float):
    """Build ORM expression for temporal + confidence scoring."""
    from api.models.memory import MemoryRecord as M
    age_hours = func.timestampdiff(text("HOUR"), M.observed_at, func.now())
    age_days = func.timestampdiff(text("DAY"), M.observed_at, func.now())
    return (
        w_time * func.exp(-age_hours / decay_hours) +
        w_conf * (M.initial_confidence * func.exp(-age_days / half_life))
    ).label("relevance")


def _safe_exp(x: float) -> float:
    """exp() clamped to avoid overflow."""
    return math.exp(max(-500.0, min(500.0, x)))


@dataclass
class _Candidate:
    memory_id: str
    content: str
    memory_type: str
    initial_confidence: float
    observed_at: object
    session_id: Optional[str]
    trust_tier: str = "T3"
    keyword_matched: bool = False
    l2_dist: Optional[float] = None


@dataclass
class _PhaseStats:
    keyword_attempted: bool = False
    keyword_hit: bool = False
    keyword_error: Optional[str] = None
    vector_attempted: bool = False
    vector_hit: bool = False
    vector_error: Optional[str] = None


class MemoryRetriever(DbConsumer):
    """Query-aware hybrid retrieval over the memories table.

    Confidence decay is computed at query time only — never mutated in DB.
    """

    def __init__(
        self,
        db_factory: DbFactory,
        decay_hours: float = 720.0,
        half_life_days: float = 30.0,
        metrics: Optional[MemoryMetrics] = None,
    ):
        super().__init__(db_factory)
        self.decay_hours = decay_hours
        self.half_life_days = half_life_days
        self._metrics = metrics or MemoryMetrics()

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
        start = time.time() if explain else 0
        stats = RetrievalStats() if explain else None

        weights = weights or TASK_WEIGHTS.get(task_hint or "default", TASK_WEIGHTS["default"])
        memory_types = memory_types or [MemoryType.SEMANTIC, MemoryType.PROCEDURAL, MemoryType.PROFILE]
        type_values = tuple(t.value for t in memory_types)

        base_params = {
            "uid": user_id, "types": type_values,
            "decay_hours": self.decay_hours, "half_life": self.half_life_days,
            "session_id": session_id,
            "include_cross": include_cross_session,
        }

        with Timer("retriever_retrieve", self._metrics):
            with self._db() as db:
                # Phase 1
                p1_start = time.time() if explain else 0
                phase1, p1_stats = self._phase1(
                    db, query_text, base_params, weights,
                    limit * 2 if query_embedding else limit,
                )
            if stats:
                stats.keyword_attempted = p1_stats.keyword_attempted
                stats.keyword_hit = p1_stats.keyword_hit
                stats.keyword_error = p1_stats.keyword_error
                stats.phase1_candidates = len(phase1)
                stats.phase1_ms = (time.time() - p1_start) * 1000

            if not query_embedding:
                memories = [self._to_memory(c, user_id) for c in phase1[:limit]]
                if stats:
                    stats.final_count = len(memories)
                    stats.total_ms = (time.time() - start) * 1000
                return memories, stats

            # Phase 2
            p2_start = time.time() if explain else 0
            phase2, p2_stats = self._phase2(db, query_embedding, base_params, limit * 2)
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
        self, db, query_text: str, base_params: dict,
        weights: RetrievalWeights, limit: int,
    ) -> tuple[list[_Candidate], _PhaseStats]:
        total = weights.temporal + weights.confidence
        w_time = weights.temporal / total if total > 0 else 0.5
        w_conf = weights.confidence / total if total > 0 else 0.5
        stats = _PhaseStats()

        from api.models.memory import MemoryRecord as M

        rel = _relevance_expr(w_time, w_conf, self.decay_hours, self.half_life_days)
        type_values = base_params["types"]
        uid = base_params["uid"]
        session_id = base_params["session_id"]
        include_cross = base_params["include_cross"]

        def _base_query():
            q = (
                db.query(M.memory_id, M.content, M.memory_type, M.initial_confidence,
                         M.observed_at, M.session_id, M.trust_tier, rel)
                .filter(M.user_id == uid, M.is_active == 1, M.memory_type.in_(type_values))
            )
            if include_cross:
                from sqlalchemy import or_
                q = q.filter(or_(M.session_id == session_id, M.session_id.is_(None)))
            else:
                q = q.filter(M.session_id == session_id)
            return q

        if query_text and query_text.strip():
            stats.keyword_attempted = True
            try:
                from matrixone.sqlalchemy_ext import boolean_match
                ft = boolean_match("content").must(query_text)
                rows = _base_query().filter(ft).order_by(rel.desc()).limit(limit).all()
                if rows:
                    self._metrics.increment("retrieval_keyword_hits")
                    stats.keyword_hit = True
                    return [_Candidate(r.memory_id, r.content, r.memory_type, r.initial_confidence,
                                       r.observed_at, r.session_id, trust_tier=r.trust_tier or "T3",
                                       keyword_matched=True) for r in rows], stats
            except Exception as e:
                logger.debug("Keyword search failed: %s", e)
                self._metrics.increment("retrieval_keyword_errors")
                stats.keyword_error = str(e)

        rows = _base_query().order_by(rel.desc()).limit(limit).all()
        self._metrics.increment("retrieval_fallback_hits")
        return [_Candidate(r.memory_id, r.content, r.memory_type, r.initial_confidence,
                           r.observed_at, r.session_id, trust_tier=r.trust_tier or "T3") for r in rows], stats

    def _phase2(
        self, db, query_embedding: list[float], base_params: dict, limit: int,
    ) -> tuple[list[_Candidate], _PhaseStats]:
        stats = _PhaseStats(vector_attempted=True)

        from matrixone.sqlalchemy_ext import l2_distance

        from api.models.memory import MemoryRecord as M

        dist_expr = l2_distance(M.embedding, query_embedding).label("l2_dist")
        uid = base_params["uid"]
        type_values = base_params["types"]
        session_id = base_params["session_id"]
        include_cross = base_params["include_cross"]

        try:
            q = (
                db.query(M.memory_id, M.content, M.memory_type, M.initial_confidence,
                         M.observed_at, M.session_id, M.trust_tier, dist_expr)
                .filter(M.user_id == uid, M.is_active == 1,
                        M.memory_type.in_(type_values), M.embedding.isnot(None))
            )
            if include_cross:
                from sqlalchemy import or_
                q = q.filter(or_(M.session_id == session_id, M.session_id.is_(None)))
            else:
                q = q.filter(M.session_id == session_id)
            rows = q.order_by("l2_dist").limit(limit).all()
            self._metrics.increment("retrieval_vector_hits")
            stats.vector_hit = bool(rows)
            return [_Candidate(r.memory_id, r.content, r.memory_type, r.initial_confidence,
                               r.observed_at, r.session_id, trust_tier=r.trust_tier or "T3",
                               l2_dist=float(r.l2_dist)) for r in rows], stats
        except Exception as e:
            logger.warning("Vector search failed: %s", e)
            self._metrics.increment("retrieval_vector_errors")
            stats.vector_error = str(e)
            return [], stats

    def _merge(
        self, phase1: list[_Candidate], phase2: list[_Candidate],
        user_id: str, weights: RetrievalWeights, limit: int,
    ) -> list[Memory]:
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
                tier_half_life = TRUST_TIER_HALF_LIVES.get(TrustTier(c.trust_tier), self.half_life_days)
                conf_score = c.initial_confidence * _safe_exp(-age_days / tier_half_life)
            else:
                time_score, conf_score = 0.0, c.initial_confidence

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
            initial_confidence=c.initial_confidence, session_id=c.session_id,
            observed_at=c.observed_at,
            trust_tier=TrustTier(c.trust_tier) if c.trust_tier else TrustTier.T3_INFERRED,
        )
