"""Hybrid retrieval using MatrixOne native vector + fulltext search.

Implements two-path hybrid search combining:
- Semantic similarity (vector L2 distance)
- Keyword matching (fulltext search)
- Temporal decay (recency scoring)
- Causal proximity (same chain bonus)

Each path runs as a separate ORM query; results are merged and reranked in Python.
"""

from typing import Any

from sqlalchemy import literal_column, text
from sqlalchemy.sql import func

from core.db_consumer import DbConsumer, DbFactory
from core.logging_config import get_logger

logger = get_logger(__name__)


class HybridRetriever(DbConsumer):
    """MatrixOne-native hybrid retrieval for episodic and semantic memory."""

    def __init__(self, db_factory: DbFactory):
        super().__init__(db_factory)

    def retrieve_events(
        self,
        query_text: str,
        query_embedding: list[float],
        session_id: str,
        current_chain_id: str | None = None,
        limit: int = 20,
        weights: dict[str, float] | None = None,
    ) -> list[dict[str, Any]]:
        """Retrieve relevant events using hybrid search (vector + fulltext).

        Two-path approach:
        1. Vector search: semantic similarity with temporal/causal scoring
        2. Fulltext search: keyword matching
        Then rerank in Python combining both scores.
        """
        if weights is None:
            weights = self._current_weights()

        from matrixone.sqlalchemy_ext import l2_distance

        from api.models.agent import Event
        from api.models.context import EventEmbedding

        events_by_id: dict[str, dict[str, Any]] = {}

        with self._db() as db:
            # 1. Vector search (semantic + temporal + causal)
            try:
                dist = l2_distance(EventEmbedding.embedding, query_embedding)
                sem_score = (weights["semantic"] * (1.0 / (1.0 + dist))).label("sem")
                temp_score = (weights["temporal"] * func.exp(
                    -func.timestampdiff(text("HOUR"), Event.created_at, func.now()) / 24.0
                )).label("temp")

                rows = (
                    db.query(
                        Event.event_id, Event.session_id, Event.event_type,
                        Event.content, Event.created_at, Event.causal_chain_id,
                        Event.parent_event_id, Event.event_metadata,
                        sem_score, temp_score,
                    )
                    .join(EventEmbedding, Event.event_id == EventEmbedding.event_id)
                    .filter(Event.session_id == session_id)
                    .order_by(sem_score.desc())
                    .limit(limit)
                    .all()
                )
                for r in rows:
                    causal_bonus = weights["causal"] if (current_chain_id and r.causal_chain_id == current_chain_id) else 0.0
                    events_by_id[r.event_id] = {
                        "event_id": r.event_id,
                        "session_id": r.session_id,
                        "event_type": r.event_type,
                        "content": r.content,
                        "created_at": r.created_at.isoformat() if r.created_at else None,
                        "causal_chain_id": r.causal_chain_id,
                        "parent_event_id": r.parent_event_id,
                        "metadata": r.event_metadata,
                        "vector_score": float(r.sem) + float(r.temp) + causal_bonus,
                        "keyword_score": 0.0,
                    }
            except Exception as e:
                logger.warning("Vector search failed: %s", e)

            # 2. Fulltext search
            try:
                from matrixone.sqlalchemy_ext import boolean_match
                ft = boolean_match("content", "session_id").must(query_text)
                # compile() returns a complete SQL literal with the query text
                # inlined by the SDK — no bind-parameter placeholders.  Escaping
                # is the SDK's responsibility; manual format()+replace() would be
                # the classic SQL-injection anti-pattern.
                ft_sql = ft.compile()
                assert ft_sql.startswith("MATCH("), f"Unexpected ft.compile() output: {ft_sql!r}"
                ft_score_col = literal_column(ft_sql).label("ft_score")
                rows = (
                    db.query(
                        Event.event_id, Event.session_id, Event.event_type,
                        Event.content, Event.created_at, Event.causal_chain_id,
                        Event.parent_event_id, Event.event_metadata,
                        ft_score_col,
                    )
                    .filter(ft, Event.session_id == session_id)
                    .limit(limit)
                    .all()
                )
                for r in rows:
                    # Normalize BM25 to 0-1, then scale by keyword weight
                    raw_score = float(r.ft_score) if r.ft_score else 0.0
                    norm_score = raw_score / (raw_score + 1.0) if raw_score > 0 else 0.0
                    kw_score = weights["keyword"] * norm_score
                    if r.event_id in events_by_id:
                        events_by_id[r.event_id]["keyword_score"] = kw_score
                    else:
                        events_by_id[r.event_id] = {
                            "event_id": r.event_id,
                            "session_id": r.session_id,
                            "event_type": r.event_type,
                            "content": r.content,
                            "created_at": r.created_at.isoformat() if r.created_at else None,
                            "causal_chain_id": r.causal_chain_id,
                            "parent_event_id": r.parent_event_id,
                            "metadata": r.event_metadata,
                            "vector_score": 0.0,
                            "keyword_score": kw_score,
                        }
            except Exception as e:
                logger.warning("Fulltext search failed: %s", e)

        # 3. Rerank
        events = list(events_by_id.values())
        for ev in events:
            ev["relevance_score"] = ev.pop("vector_score") + ev.pop("keyword_score")
        events.sort(key=lambda x: x["relevance_score"], reverse=True)
        events = events[:limit]

        if events:
            logger.info("Hybrid retrieval: %d events, top score: %.3f", len(events), events[0]["relevance_score"])
        return events

    def update_weights_from_feedback(
        self,
        session_id: str,
        quality_score: float,
        retrieval_metadata: dict[str, float],
        learning_rate: float = 0.05,
    ) -> dict[str, float]:
        """Adjust retrieval weights based on quality feedback.

        Called after a session ends with a quality_score (0-1) from the
        evaluation gate or implicit feedback. Uses the retrieval_metadata
        recorded at retrieval time (vector_fraction, keyword_fraction) to
        nudge weights toward the mix that correlated with high quality.

        Args:
            session_id: Session that produced the feedback signal.
            quality_score: 0-1 quality rating (higher = better).
            retrieval_metadata: Dict with keys "vector_fraction" and
                "keyword_fraction" — the actual mix used during retrieval.
            learning_rate: Step size for weight update (default 0.05).

        Returns:
            Updated weights dict (also stored in-memory for next retrieval).
        """
        if not (0.0 <= quality_score <= 1.0):
            logger.warning("Invalid quality_score %.3f for session %s, skipping weight update", quality_score, session_id)
            return self._current_weights()

        vector_frac = retrieval_metadata.get("vector_fraction", 0.5)
        keyword_frac = retrieval_metadata.get("keyword_fraction", 0.5)

        # Reward signal: positive when quality > 0.5, negative otherwise
        reward = quality_score - 0.5

        # Nudge semantic/keyword weights proportional to their usage fraction
        w = self._current_weights()
        w["semantic"] = max(0.05, min(0.80, w["semantic"] + learning_rate * reward * vector_frac))
        w["keyword"]  = max(0.05, min(0.80, w["keyword"]  + learning_rate * reward * keyword_frac))

        # Renormalize all four weights so they sum to 1.
        # temporal and causal are included in the denominator so the total stays
        # at 1.0, but they are not directly nudged — their share changes only as
        # a side-effect of semantic/keyword moving.  This is intentional: we have
        # no direct signal for temporal/causal quality, so we let them scale
        # proportionally rather than holding them fixed.
        total = w["semantic"] + w["keyword"] + w["temporal"] + w["causal"]
        w = {k: v / total for k, v in w.items()}

        self._weights = w
        logger.info(
            "Weight update for session %s (quality=%.2f): semantic=%.3f keyword=%.3f temporal=%.3f causal=%.3f",
            session_id, quality_score, w["semantic"], w["keyword"], w["temporal"], w["causal"],
        )
        return w

    def _current_weights(self) -> dict[str, float]:
        """Return current weights (instance-level override or defaults)."""
        return dict(getattr(self, "_weights", {
            "semantic": 0.35, "keyword": 0.25, "temporal": 0.20, "causal": 0.20,
        }))

    def retrieve_knowledge(
        self,
        query_text: str,
        query_embedding: list[float],
        user_id: str,
        limit: int = 10,
        confidence_threshold: float = 0.3,
        weights: dict[str, float] | None = None,
    ) -> list[dict[str, Any]]:
        """Retrieve relevant knowledge entries using hybrid search."""
        if weights is None:
            weights = {"semantic": 0.5, "keyword": 0.3, "confidence": 0.2}
        required_keys = {"semantic", "keyword", "confidence"}
        if not required_keys.issubset(weights.keys()):
            logger.error("Invalid weights dict, missing keys: %s", required_keys - weights.keys())
            return []

        from matrixone.sqlalchemy_ext import l2_distance

        from skills.knowledge.models import SkKnowledgeEntry as K

        entries: list[dict[str, Any]] = []
        tracked_ids: list[str] = []

        with self._db() as db:
            # Vector + confidence scoring
            try:
                sem = (weights["semantic"] * (1.0 / (1.0 + l2_distance(K.embedding, query_embedding)))).label("sem")
                conf = (weights["confidence"] * K.confidence).label("conf")
                rows = (
                    db.query(
                        K.entry_id, K.category, K.key_name, K.value,
                        K.confidence, K.trust_tier, K.created_at, K.last_validated_at,
                        sem, conf,
                    )
                    .filter(
                        K.user_id == user_id,
                        K.confidence > confidence_threshold,
                        K.embedding.isnot(None),
                    )
                    .order_by(sem.desc())
                    .limit(limit)
                    .all()
                )
                entries_by_id: dict[str, dict[str, Any]] = {}
                for r in rows:
                    entries_by_id[r.entry_id] = {
                        "entry_id": r.entry_id, "category": r.category,
                        "key_name": r.key_name, "value": r.value,
                        "confidence": float(r.confidence), "trust_tier": r.trust_tier,
                        "created_at": r.created_at.isoformat() if r.created_at else None,
                        "last_validated_at": r.last_validated_at.isoformat() if r.last_validated_at else None,
                        "relevance_score": float(r.sem) + float(r.conf),
                    }

                # Fulltext boost — add BM25-weighted keyword score to matching entries
                try:
                    from matrixone.sqlalchemy_ext import boolean_match
                    ft = boolean_match("value").must(query_text)
                    # compile() returns a complete SQL literal — see events
                    # retrieval comment above for rationale.
                    ft_sql = ft.compile()
                    assert ft_sql.startswith("MATCH("), f"Unexpected ft.compile() output: {ft_sql!r}"
                    ft_score_col = literal_column(ft_sql).label("ft_score")
                    ft_rows = (
                        db.query(K.entry_id, ft_score_col)
                        .filter(ft, K.user_id == user_id, K.confidence > confidence_threshold)
                        .limit(limit)
                        .all()
                    )
                    for r in ft_rows:
                        raw = float(r.ft_score) if r.ft_score else 0.0
                        norm = raw / (raw + 1.0) if raw > 0 else 0.0
                        kw_boost = weights["keyword"] * norm
                        if r.entry_id in entries_by_id:
                            entries_by_id[r.entry_id]["relevance_score"] += kw_boost
                    # Batch-fetch fulltext-only hits
                    new_ids = [r.entry_id for r in ft_rows if r.entry_id not in entries_by_id]
                    ft_scores = {r.entry_id: float(r.ft_score) for r in ft_rows}
                    if new_ids:
                        full_rows = db.query(K).filter(K.entry_id.in_(new_ids)).all()
                        for full in full_rows:
                            raw = ft_scores.get(full.entry_id, 0.0)
                            norm = raw / (raw + 1.0) if raw > 0 else 0.0
                            entries_by_id[full.entry_id] = {
                                "entry_id": full.entry_id, "category": full.category,
                                "key_name": full.key_name, "value": full.value,
                                "confidence": float(full.confidence), "trust_tier": full.trust_tier,
                                "created_at": full.created_at.isoformat() if full.created_at else None,
                                "last_validated_at": full.last_validated_at.isoformat() if full.last_validated_at else None,
                                "relevance_score": weights["keyword"] * norm + weights["confidence"] * float(full.confidence),
                            }
                except Exception as e:
                    logger.warning("Knowledge fulltext search failed (non-fatal): %s", e)
                    try:
                        db.rollback()
                    except Exception:
                        pass

                entries = sorted(entries_by_id.values(), key=lambda x: x["relevance_score"], reverse=True)[:limit]
                if entries:
                    logger.info("Knowledge retrieval: %d entries, top score: %.3f", len(entries), entries[0]["relevance_score"])
            except Exception as e:
                logger.error("Knowledge retrieval failed: %s", e)

            # Access tracking — collect IDs, fire-and-forget after read session
            if entries:
                tracked_ids = [e["entry_id"] for e in entries]

            # 1-hop graph expansion
            if entries:
                try:
                    from skills.knowledge.api import expand_with_graph
                    seed_ids = [e["entry_id"] for e in entries[:5]]
                    expanded_ids = expand_with_graph(db, seed_ids, limit_per_entry=2)
                    if expanded_ids:
                        existing_ids = {e["entry_id"] for e in entries}
                        new_ids = [eid for eid in expanded_ids if eid not in existing_ids]
                        if new_ids:
                            graph_rows = (
                                db.query(K)
                                .filter(K.entry_id.in_(new_ids), K.user_id == user_id, K.confidence > confidence_threshold)
                                .all()
                            )
                            for r in graph_rows:
                                entries.append({
                                    "entry_id": r.entry_id, "category": r.category,
                                    "key_name": r.key_name, "value": r.value,
                                    "confidence": float(r.confidence), "trust_tier": r.trust_tier,
                                    "created_at": r.created_at.isoformat() if r.created_at else None,
                                    "last_validated_at": r.last_validated_at.isoformat() if r.last_validated_at else None,
                                    "relevance_score": 0.0,
                                    "source": "graph_expansion",
                                })
                            if graph_rows:
                                tracked_ids.extend(r.entry_id for r in graph_rows)
                except Exception as e:
                    logger.warning("Knowledge graph expansion failed (non-fatal): %s", e)

        # Fire-and-forget access tracking outside the read session
        if tracked_ids:
            from skills.knowledge.api import update_access_tracking
            update_access_tracking(self._db, tracked_ids)

        return entries
