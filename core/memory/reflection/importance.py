"""ImportanceScorer — multi-signal heuristic scoring for reflection candidates.

Stateless, no LLM calls. Both tabular and graph backends map their signals
to the same 4 dimensions with identical weights.

See docs/design/memory/graph-memory.md §4.4
See docs/design/memory/tabular-memory.md "Importance Scoring (Tabular Adaptation)"
"""

from __future__ import annotations

from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from core.memory.interfaces import ReflectionCandidate

# Weight per signal (must sum to 1.0)
W_CENTRALITY = 0.25
W_CROSS_SESSION = 0.25
W_CONTRADICTION = 0.30
W_RECURRENCE = 0.20

# Thresholds
IMMEDIATE_THRESHOLD = 0.7  # event-triggered reflection
DAILY_THRESHOLD = 0.5      # queued for daily reflection


class ImportanceScorer:
    """Score reflection candidates by importance. No DB, no LLM."""

    def __init__(
        self,
        w_centrality: float = W_CENTRALITY,
        w_cross_session: float = W_CROSS_SESSION,
        w_contradiction: float = W_CONTRADICTION,
        w_recurrence: float = W_RECURRENCE,
    ):
        self._w = {
            "centrality": w_centrality,
            "cross_session": w_cross_session,
            "contradiction": w_contradiction,
            "recurrence": w_recurrence,
        }

    def score(self, candidate: ReflectionCandidate) -> float:
        """Score a candidate cluster. Returns 0.0-1.0."""
        centrality = self._centrality(candidate)
        cross_session = self._cross_session(candidate)
        contradiction = self._contradiction(candidate)
        recurrence = self._recurrence(candidate)

        return (
            self._w["centrality"] * centrality
            + self._w["cross_session"] * cross_session
            + self._w["contradiction"] * contradiction
            + self._w["recurrence"] * recurrence
        )

    def _centrality(self, c: ReflectionCandidate) -> float:
        """Cluster size as proxy for structural centrality (tabular).

        Graph backend overrides with activation energy.
        """
        return min(len(c.memories) / 5.0, 1.0)

    def _cross_session(self, c: ReflectionCandidate) -> float:
        """Distinct session count normalized to [0, 1]."""
        return min(len(set(c.session_ids)) / 3.0, 1.0)

    def _contradiction(self, c: ReflectionCandidate) -> float:
        """Contradiction signal from candidate boost."""
        if c.signal == "contradiction":
            return 1.0
        return min(c.importance_boost / 0.3, 1.0) if c.importance_boost > 0 else 0.0

    def _recurrence(self, c: ReflectionCandidate) -> float:
        """Recurrence frequency."""
        if c.signal == "summary_recurrence":
            return min(len(c.memories) / 3.0, 1.0)
        return min(len(c.memories) / 5.0, 1.0)
