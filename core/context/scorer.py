"""Configurable relevance scorer for context selection.

Implements multi-signal relevance scoring with task-aware weights.
Includes topic shift detection to suppress stale context on topic changes.
"""

import time
from dataclasses import dataclass
from typing import Any

from core.context.manager import TaskType
from core.db_consumer import DbConsumer, DbFactory
from core.logging_config import get_logger
from core.utils.similarity import cosine_similarity

logger = get_logger(__name__)


@dataclass
class TopicShiftConfig:
    """Configurable parameters for topic shift detection.

    Loaded from ``infra_configs`` table (key ``topic_shift_config``).
    Defaults are used when no DB override exists.
    The ContextBudgetTuner (or a future TopicShiftTuner) can update these
    through the standard Observe → Diagnose → Propose → Gate → Deploy loop.
    """

    threshold: float = 0.5       # shift_score below this → no adjustment
    temporal_floor: float = 0.05 # minimum temporal weight after adjustment
    causal_floor: float = 0.05   # minimum causal weight after adjustment
    semantic_ceiling: float = 0.8  # maximum semantic weight after adjustment

    @staticmethod
    def from_dict(d: dict[str, Any]) -> "TopicShiftConfig":
        """Parse from JSON dict (as stored in configs table)."""
        return TopicShiftConfig(
            threshold=float(d.get("threshold", 0.5)),
            temporal_floor=float(d.get("temporal_floor", 0.05)),
            causal_floor=float(d.get("causal_floor", 0.05)),
            semantic_ceiling=float(d.get("semantic_ceiling", 0.8)),
        )

    def to_dict(self) -> dict[str, float]:
        return {
            "threshold": self.threshold,
            "temporal_floor": self.temporal_floor,
            "causal_floor": self.causal_floor,
            "semantic_ceiling": self.semantic_ceiling,
        }


# Module-level default; overridden by DB config when available
_DEFAULT_TOPIC_SHIFT_CONFIG = TopicShiftConfig()


@dataclass
class ScoringWeights:
    """Weights for different relevance signals."""

    semantic: float = 0.4  # Embedding similarity
    temporal: float = 0.2  # Recency
    causal: float = 0.3  # Causal chain membership
    keyword: float = 0.1  # Exact keyword match

    def __post_init__(self):
        """Validate weights sum to 1.0."""
        total = self.semantic + self.temporal + self.causal + self.keyword
        if abs(total - 1.0) > 0.01:
            raise ValueError(f"Weights must sum to 1.0, got {total}")

    def adjust_for_topic_shift(
        self, shift_score: float, config: TopicShiftConfig | None = None,
    ) -> "ScoringWeights":
        """Return new weights adjusted for topic shift.

        When the user switches topics, temporal and causal signals become noise
        (they boost old-topic events). Redistribute their weight to semantic
        so the scorer favours content relevant to the *new* query.

        Args:
            shift_score: 0.0 = same topic, 1.0 = completely new topic.
                         Values below config.threshold are treated as "same topic".
            config: Tunable parameters. Loaded from DB by RelevanceScorer;
                    defaults used if None.

        Returns:
            New ScoringWeights instance (original is not mutated).
        """
        cfg = config or _DEFAULT_TOPIC_SHIFT_CONFIG
        if shift_score < cfg.threshold:
            return self
        new_temporal = max(cfg.temporal_floor, self.temporal * (1 - shift_score))
        new_causal = max(cfg.causal_floor, self.causal * (1 - shift_score))
        new_keyword = self.keyword
        new_semantic = min(cfg.semantic_ceiling, 1.0 - new_temporal - new_causal - new_keyword)
        return ScoringWeights(
            semantic=new_semantic,
            temporal=new_temporal,
            causal=new_causal,
            keyword=new_keyword,
        )


# Task-specific weight profiles
TASK_WEIGHTS = {
    TaskType.CODE_REVIEW: ScoringWeights(
        semantic=0.5,  # Code similarity is key
        temporal=0.1,  # Less emphasis on recency
        causal=0.3,  # Follow code review chains
        keyword=0.1,
    ),
    TaskType.PLANNING: ScoringWeights(
        semantic=0.3,
        temporal=0.3,  # Recent discussions matter
        causal=0.3,  # Follow planning threads
        keyword=0.1,
    ),
    TaskType.DEBUGGING: ScoringWeights(
        semantic=0.4,
        temporal=0.2,
        causal=0.3,  # Follow error chains
        keyword=0.1,
    ),
    TaskType.GENERAL: ScoringWeights(
        semantic=0.4,
        temporal=0.2,
        causal=0.3,
        keyword=0.1,
    ),
}


class RelevanceScorer(DbConsumer):
    """Multi-signal relevance scorer with configurable weights."""

    def __init__(self, db_factory: DbFactory, embeddings, weights: ScoringWeights | None = None):
        """Initialize scorer.

        Args:
            db_factory: Callable returning a new SQLAlchemy Session
            embeddings: Embedding service
            weights: Custom weights (default: GENERAL task weights)
        """
        super().__init__(db_factory)
        self.embeddings = embeddings
        self.weights = weights or TASK_WEIGHTS[TaskType.GENERAL]
        # Per-instance cache — not shared across instances (avoids race conditions
        # in parallel requests and pytest-xdist workers).
        self._topic_shift_config: TopicShiftConfig | None = None
        self._topic_shift_config_ts: float = 0.0
        self._TOPIC_SHIFT_CONFIG_TTL: float = 60.0  # seconds

    def _load_topic_shift_config(self) -> TopicShiftConfig:
        """Load TopicShiftConfig from configs table with TTL cache.

        Same pattern as ContextManager._load_budget_ratios and
        ToolRegistry selection.
        """
        now = time.monotonic()
        if self._topic_shift_config is not None and (now - self._topic_shift_config_ts) < self._TOPIC_SHIFT_CONFIG_TTL:
            return self._topic_shift_config

        try:
            import json as _json
            with self._db() as db:
                from api.models import Config
                row = db.query(Config.value).filter(Config.key_name == "topic_shift_config").first()
            if row:
                raw = _json.loads(row[0]) if isinstance(row[0], str) else row[0]
                self._topic_shift_config = TopicShiftConfig.from_dict(raw)
            else:
                self._topic_shift_config = _DEFAULT_TOPIC_SHIFT_CONFIG
        except Exception as e:
            logger.debug("Failed to load topic_shift_config, using defaults: %s", e)
            self._topic_shift_config = _DEFAULT_TOPIC_SHIFT_CONFIG
        self._topic_shift_config_ts = now
        return self._topic_shift_config

    def detect_topic_shift(
        self,
        query: str,
        recent_events: list[dict[str, Any]],
    ) -> float:
        """Detect topic shift by comparing query embedding to recent conversation.

        Compares the query embedding against the mean embedding of the last few
        events.  Cost: one embed call for the query only — recent event embeddings
        are looked up from the event_embeddings table (already computed by
        EmbeddingWorker on ingest).  Falls back to inline embedding if stored
        embeddings are unavailable.

        Args:
            query: Current user query.
            recent_events: Last N events (must have ``content`` and ``event_id`` keys).

        Returns:
            0.0 (same topic) to 1.0 (completely new topic).
        """
        if not recent_events:
            return 0.0

        try:
            query_emb = self.embeddings.embed_text(query)
        except Exception as e:
            logger.debug("Topic shift detection skipped (embed failed): %s", e)
            return 0.0

        # Collect embeddings for recent events.
        # Prefer stored embeddings (zero cost); fall back to inline embed.
        recent_embs: list[list[float]] = []
        for event in recent_events[-3:]:
            if not event.get("content"):
                continue
            # Try stored embedding first (from event_embeddings table)
            stored = self._get_stored_embedding(event.get("event_id"))
            if stored is not None:
                recent_embs.append(stored)
                continue
            # Fallback: embed inline (costs one API call per event)
            try:
                recent_embs.append(self.embeddings.embed_text(event["content"]))
            except Exception:
                continue

        if not recent_embs:
            return 0.0

        dim = len(recent_embs[0])
        mean_emb = [sum(e[i] for e in recent_embs) / len(recent_embs) for i in range(dim)]

        similarity = cosine_similarity(query_emb, mean_emb)
        shift = max(0.0, 1.0 - similarity)
        logger.debug("Topic shift score: %.3f (similarity=%.3f, %d recent events)", shift, similarity, len(recent_embs))
        return shift

    def _get_stored_embedding(self, event_id: str | None) -> list[float] | None:
        """Look up a pre-computed embedding from event_embeddings table.

        Returns None if not found or on any error (caller falls back to inline embed).
        """
        if not event_id:
            return None
        try:
            from api.models.context import EventEmbedding
            with self._db() as db:
                row = db.query(EventEmbedding.embedding).filter(
                    EventEmbedding.event_id == event_id
                ).first()
            if row and row[0]:
                import json
                raw = row[0]
                if isinstance(raw, str):
                    return json.loads(raw)
                return list(raw)
        except Exception:
            pass
        return None

    def score_candidates(
        self,
        query: str,
        candidates: list[dict[str, Any]],
        session_id: str,
        task_type: TaskType = TaskType.GENERAL,
        topic_shift: float | None = None,
    ) -> list[tuple[dict[str, Any], float, dict[str, float]]]:
        """Score candidates by relevance.

        Args:
            query: User query
            candidates: List of candidate events
            session_id: Session ID
            task_type: Task type for weight selection
            topic_shift: Pre-computed topic shift score (0-1). If provided,
                         weights are adjusted to suppress stale context.

        Returns:
            List of (candidate, total_score, signal_scores)
        """
        # Input validation
        if not query or not query.strip():
            logger.warning("Empty query provided to scorer")
            return [(c, 0.0, {}) for c in candidates]

        if not candidates:
            logger.debug("No candidates to score")
            return []

        if not session_id or not session_id.strip():
            logger.warning("Empty session_id provided to scorer")
            return [(c, 0.0, {}) for c in candidates]

        # Use task-specific weights, adjusted for topic shift
        weights = TASK_WEIGHTS.get(task_type, self.weights)
        if topic_shift is not None:
            ts_config = self._load_topic_shift_config()
            weights = weights.adjust_for_topic_shift(topic_shift, config=ts_config)

        # Generate query embedding
        try:
            query_embedding = self.embeddings.embed_text(query)
        except Exception as e:
            logger.error(f"Failed to generate query embedding: {e}")
            # Fallback: score without semantic signal
            return [(c, 0.0, {"error": 1.0}) for c in candidates]

        # Get semantic scores
        try:
            semantic_results = self.embeddings.search_similar(
                query_embedding, limit=len(candidates), session_id=session_id
            )
            distance_map = {r["event_id"]: r["distance"] for r in semantic_results}
        except Exception as e:
            logger.error(f"Failed to search similar embeddings: {e}")
            distance_map = {}

        # Get recent causal chains
        try:
            recent_chains = self._get_recent_chains(session_id)
        except Exception as e:
            logger.error(f"Failed to get recent chains: {e}")
            recent_chains = set()

        # Score each candidate
        scored = []
        for candidate in candidates:
            try:
                signal_scores = self._compute_signals(
                    candidate, query, distance_map, recent_chains, weights
                )
                total_score = sum(signal_scores.values())
                scored.append((candidate, total_score, signal_scores))
            except Exception as e:
                logger.error(f"Failed to score candidate {candidate.get('event_id')}: {e}")
                # Include with zero score
                scored.append((candidate, 0.0, {"error": 1.0}))

        # Sort by score descending
        scored.sort(key=lambda x: x[1], reverse=True)
        return scored

    def _get_recent_chains(self, session_id: str, limit: int = 5) -> set[str]:
        """Get recent causal chain IDs."""
        from api.models import Event
        from sqlalchemy import func
        
        with self._db() as db:
            chains = db.query(
                Event.causal_chain_id,
                func.max(Event.created_at).label('last_time')
            ).filter(
                Event.session_id == session_id
            ).group_by(Event.causal_chain_id).order_by(
                func.max(Event.created_at).desc()
            ).limit(limit).all()
        
        return {row.causal_chain_id for row in chains}

    def _compute_signals(
        self,
        candidate: dict[str, Any],
        query: str,
        distance_map: dict[str, float],
        recent_chains: set[str],
        weights: ScoringWeights,
    ) -> dict[str, float]:
        """Compute individual signal scores."""
        event_id = candidate["event_id"]

        # 1. Semantic score (L2 distance → similarity)
        distance = distance_map.get(event_id, 999.0)
        semantic_raw = 1.0 / (1.0 + distance)
        semantic_score = semantic_raw * weights.semantic

        # 2. Temporal score (exponential decay)
        created_at = candidate["created_at"]
        if isinstance(created_at, str):
            from datetime import datetime
            try:
                created_at = datetime.fromisoformat(created_at)
            except (ValueError, TypeError):
                created_at = None
        age_hours = (time.time() - created_at.timestamp()) / 3600 if created_at else 24.0
        temporal_raw = 0.5 ** (age_hours / 24.0)  # Half-life of 24 hours
        temporal_score = temporal_raw * weights.temporal

        # 3. Causal score (chain membership)
        chain_id = candidate.get("causal_chain_id")
        if chain_id and chain_id in recent_chains:
            causal_raw = 1.0
        else:
            causal_raw = 0.0
        causal_score = causal_raw * weights.causal

        # 4. Keyword score (exact match)
        query_lower = query.lower()
        content_lower = candidate["content"].lower()
        keyword_raw = 1.0 if query_lower in content_lower else 0.0
        keyword_score = keyword_raw * weights.keyword

        return {
            "semantic": semantic_score,
            "temporal": temporal_score,
            "causal": causal_score,
            "keyword": keyword_score,
        }


def create_scorer_for_task(db_factory: DbFactory, embeddings, task_type: TaskType) -> RelevanceScorer:
    """Factory function to create task-specific scorer."""
    weights = TASK_WEIGHTS[task_type]
    return RelevanceScorer(db_factory, embeddings, weights)
