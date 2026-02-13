"""Configurable relevance scorer for context selection.

Implements multi-signal relevance scoring with task-aware weights.
"""

import time
from dataclasses import dataclass
from typing import Any

from core.context.manager import TaskType
from core.logging_config import get_logger

logger = get_logger(__name__)


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


class RelevanceScorer:
    """Multi-signal relevance scorer with configurable weights."""

    def __init__(self, db, embeddings, weights: ScoringWeights | None = None):
        """Initialize scorer.

        Args:
            db: SQLAlchemy Session
            embeddings: Embedding service
            weights: Custom weights (default: GENERAL task weights)
        """
        self.db = db
        self.embeddings = embeddings
        self.weights = weights or TASK_WEIGHTS[TaskType.GENERAL]

    def score_candidates(
        self,
        query: str,
        candidates: list[dict[str, Any]],
        session_id: str,
        task_type: TaskType = TaskType.GENERAL,
    ) -> list[tuple[dict[str, Any], float, dict[str, float]]]:
        """Score candidates by relevance.

        Args:
            query: User query
            candidates: List of candidate events
            session_id: Session ID
            task_type: Task type for weight selection

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

        # Use task-specific weights
        weights = TASK_WEIGHTS.get(task_type, self.weights)

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
        
        chains = self.db.query(
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
        age_hours = (time.time() - candidate["created_at"].timestamp()) / 3600
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


def create_scorer_for_task(db, embeddings, task_type: TaskType) -> RelevanceScorer:
    """Factory function to create task-specific scorer."""
    weights = TASK_WEIGHTS[task_type]
    return RelevanceScorer(db, embeddings, weights)
