"""Importance scoring — multi-signal heuristic for reflection candidates.

Computes importance from 4 graph signals. No LLM calls.
Each backend calls this (or its own scorer) and sets candidate.importance_score
before passing to ReflectionEngine.

See docs/design/memory/graph-memory.md §4.4
"""

from __future__ import annotations

from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from core.memory.interfaces import ReflectionCandidate

# Weight per signal (sum = 1.0)
W_CENTRALITY = 0.25
W_CROSS_SESSION = 0.25
W_CONTRADICTION = 0.30
W_RECURRENCE = 0.20

# Thresholds
IMMEDIATE_THRESHOLD = 0.7  # event-triggered reflection
DAILY_THRESHOLD = 0.5      # queued for daily reflection


def score_candidate(
    candidate: ReflectionCandidate,
    activation_energy: float = 0.0,
) -> float:
    """Score a reflection candidate. Returns 0.0-1.0.

    Args:
        candidate: the candidate cluster
        activation_energy: avg activation of cluster nodes (graph-specific)
    """
    centrality = min(activation_energy, 1.0) if activation_energy > 0 else min(len(candidate.memories) / 5.0, 1.0)
    cross_session = min(len(set(candidate.session_ids)) / 3.0, 1.0)

    if candidate.signal == "contradiction":
        contradiction = 1.0
    elif any(
        getattr(m, "initial_confidence", 1.0) < 0.5
        for m in candidate.memories
    ):
        contradiction = 0.7
    else:
        contradiction = 0.0

    recurrence = min(len(candidate.memories) / 5.0, 1.0)

    return (
        W_CENTRALITY * centrality
        + W_CROSS_SESSION * cross_session
        + W_CONTRADICTION * contradiction
        + W_RECURRENCE * recurrence
    )
