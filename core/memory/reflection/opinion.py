"""OpinionEvolver — evidence-based confidence updates for scene/reflection memories.

When new evidence arrives, existing reflection-produced memories (scenes) have
their confidence adjusted: supporting evidence increases confidence, contradicting
evidence decreases it. Trust tier promotion follows confidence thresholds.

See docs/design/memory/graph-memory.md §4.5
"""

from __future__ import annotations

import logging
from dataclasses import dataclass
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from core.memory.types import Memory

logger = logging.getLogger(__name__)

# Confidence deltas
SUPPORTING_DELTA = 0.05
CONTRADICTING_DELTA = -0.10
CONFIDENCE_CAP = 0.95

# Similarity thresholds for evidence alignment
SUPPORTING_THRESHOLD = 0.8
CONTRADICTING_THRESHOLD = 0.3

# Trust tier promotion thresholds
T4_TO_T3_CONFIDENCE = 0.8
T4_TO_T3_MIN_AGE_DAYS = 7
T3_TO_T2_CONFIDENCE = 0.9  # requires human confirmation

# Quarantine threshold
QUARANTINE_THRESHOLD = 0.2


@dataclass
class OpinionUpdate:
    """Result of an opinion evolution step."""

    memory_id: str
    old_confidence: float
    new_confidence: float
    evidence_type: str  # "supporting" | "contradicting" | "neutral"
    promoted: bool = False
    quarantined: bool = False


class OpinionEvolver:
    """Evolve confidence of reflection-produced memories based on new evidence."""

    def evaluate_evidence(
        self,
        similarity: float,
        scene: Memory,
    ) -> OpinionUpdate:
        """Determine how new evidence affects a scene memory's confidence.

        Args:
            similarity: cosine similarity between new event and scene content.
            scene: the existing scene/reflection memory.

        Returns:
            OpinionUpdate with new confidence and any tier changes.
        """
        old_conf = scene.initial_confidence

        if similarity >= SUPPORTING_THRESHOLD:
            evidence_type = "supporting"
            new_conf = min(old_conf + SUPPORTING_DELTA, CONFIDENCE_CAP)
        elif similarity <= CONTRADICTING_THRESHOLD:
            evidence_type = "contradicting"
            new_conf = max(old_conf + CONTRADICTING_DELTA, 0.0)
        else:
            return OpinionUpdate(
                memory_id=scene.memory_id,
                old_confidence=old_conf,
                new_confidence=old_conf,
                evidence_type="neutral",
            )

        quarantined = new_conf < QUARANTINE_THRESHOLD
        promoted = (
            not quarantined
            and new_conf >= T4_TO_T3_CONFIDENCE
            and scene.trust_tier.value == "T4"
        )

        return OpinionUpdate(
            memory_id=scene.memory_id,
            old_confidence=old_conf,
            new_confidence=new_conf,
            evidence_type=evidence_type,
            promoted=promoted,
            quarantined=quarantined,
        )
