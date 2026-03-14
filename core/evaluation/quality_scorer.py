"""Multi-dimensional quality scoring for sessions — P1 Evaluation Loop.

Extends auto_scorer with:
- Accuracy scoring (vs golden truth)
- Latency scoring (vs SLO)
- Cost scoring (vs budget)
- User satisfaction scoring
- Trust score (from P0 confidence)
"""

from __future__ import annotations

from dataclasses import dataclass
from enum import Enum


class QualityDimension(str, Enum):
    """Quality scoring dimensions."""

    ACCURACY = "accuracy"  # Correctness vs golden truth
    LATENCY = "latency"  # Response time vs SLO
    COST = "cost"  # Token cost vs budget
    SATISFACTION = "satisfaction"  # User feedback
    TRUST = "trust"  # Confidence from P0


@dataclass
class DimensionScore:
    """Score for a single quality dimension."""

    dimension: QualityDimension
    score: float  # 0.0-1.0
    weight: float  # 0.0-1.0
    metadata: dict = None  # Dimension-specific details


@dataclass
class MultiDimensionalScore:
    """Multi-dimensional quality score for a session."""

    session_id: str
    event_id: str
    dimensions: list[DimensionScore]
    overall_score: float  # 0.0-1.0 (weighted average)
    training_eligible: bool
    golden_session: bool = False


class QualityWeights:
    """Configurable weights for quality dimensions."""

    def __init__(
        self,
        accuracy_weight: float = 0.4,
        latency_weight: float = 0.2,
        cost_weight: float = 0.15,
        satisfaction_weight: float = 0.15,
        trust_weight: float = 0.1,
    ):
        """Initialize quality weights.

        Args:
            accuracy_weight: Weight for accuracy (default: 0.4)
            latency_weight: Weight for latency (default: 0.2)
            cost_weight: Weight for cost (default: 0.15)
            satisfaction_weight: Weight for satisfaction (default: 0.15)
            trust_weight: Weight for trust (default: 0.1)
        """
        total = accuracy_weight + latency_weight + cost_weight + satisfaction_weight + trust_weight
        if abs(total - 1.0) > 0.01:
            raise ValueError(f"Weights must sum to 1.0, got {total}")

        self.weights = {
            QualityDimension.ACCURACY: accuracy_weight,
            QualityDimension.LATENCY: latency_weight,
            QualityDimension.COST: cost_weight,
            QualityDimension.SATISFACTION: satisfaction_weight,
            QualityDimension.TRUST: trust_weight,
        }

    def get_weight(self, dimension: QualityDimension) -> float:
        """Get weight for dimension."""
        return self.weights.get(dimension, 0.0)


class QualityScorer:
    """Score sessions across multiple quality dimensions."""

    def __init__(self, weights: QualityWeights | None = None):
        """Initialize scorer.

        Args:
            weights: Custom quality weights (default: standard weights)
        """
        self.weights = weights or QualityWeights()

    def score_accuracy(
        self,
        expected_output: str,
        actual_output: str,
        similarity_threshold: float = 0.8,
    ) -> DimensionScore:
        """Score accuracy against expected output.

        Args:
            expected_output: Expected/golden output
            actual_output: Actual output from agent
            similarity_threshold: Threshold for match (default: 0.8)

        Returns:
            DimensionScore for accuracy
        """
        # Simplified: exact match = 1.0, partial = 0.5, mismatch = 0.0
        if expected_output == actual_output:
            score = 1.0
        elif expected_output.lower() in actual_output.lower():
            score = 0.7
        else:
            score = 0.0

        return DimensionScore(
            dimension=QualityDimension.ACCURACY,
            score=score,
            weight=self.weights.get_weight(QualityDimension.ACCURACY),
            metadata={"expected": expected_output[:50], "actual": actual_output[:50]},
        )

    def score_latency(
        self,
        execution_time_ms: float,
        slo_ms: float = 5000,
    ) -> DimensionScore:
        """Score latency against SLO.

        Args:
            execution_time_ms: Actual execution time in milliseconds
            slo_ms: SLO target in milliseconds (default: 5000ms)

        Returns:
            DimensionScore for latency
        """
        if execution_time_ms <= slo_ms:
            score = 1.0
        elif execution_time_ms <= slo_ms * 1.5:
            score = 0.7
        else:
            score = max(0.0, 1.0 - (execution_time_ms - slo_ms) / (slo_ms * 2))

        return DimensionScore(
            dimension=QualityDimension.LATENCY,
            score=score,
            weight=self.weights.get_weight(QualityDimension.LATENCY),
            metadata={"execution_ms": execution_time_ms, "slo_ms": slo_ms},
        )

    def score_cost(
        self,
        actual_cost: float,
        budget_cost: float,
        overrun_factor: float = 1.2,
    ) -> DimensionScore:
        """Score cost against budget.

        Args:
            actual_cost: Actual cost incurred
            budget_cost: Budgeted cost
            overrun_factor: Acceptable overrun factor (default: 1.2)

        Returns:
            DimensionScore for cost
        """
        if budget_cost == 0:
            score = 1.0
        elif actual_cost <= budget_cost:
            score = 1.0
        elif actual_cost <= budget_cost * overrun_factor:
            score = 0.7
        else:
            score = max(0.0, 1.0 - (actual_cost - budget_cost) / (budget_cost * 2))

        return DimensionScore(
            dimension=QualityDimension.COST,
            score=score,
            weight=self.weights.get_weight(QualityDimension.COST),
            metadata={"actual": actual_cost, "budget": budget_cost},
        )

    def score_satisfaction(
        self,
        user_rating: float | None = None,
        feedback_sentiment: str | None = None,
    ) -> DimensionScore:
        """Score user satisfaction.

        Args:
            user_rating: User rating (0.0-1.0) if available
            feedback_sentiment: Sentiment classification (positive/neutral/negative)

        Returns:
            DimensionScore for satisfaction
        """
        if user_rating is not None:
            score = max(0.0, min(1.0, user_rating))
        elif feedback_sentiment == "positive":
            score = 0.8
        elif feedback_sentiment == "neutral":
            score = 0.5
        elif feedback_sentiment == "negative":
            score = 0.2
        else:
            score = 0.5  # Default: neutral

        return DimensionScore(
            dimension=QualityDimension.SATISFACTION,
            score=score,
            weight=self.weights.get_weight(QualityDimension.SATISFACTION),
            metadata={"rating": user_rating, "sentiment": feedback_sentiment},
        )

    def score_trust(
        self,
        confidence_score: float,
        threshold: float = 0.7,
    ) -> DimensionScore:
        """Score trust from P0 confidence.

        Args:
            confidence_score: Confidence score from P0 (0.0-1.0)
            threshold: Confidence threshold (default: 0.7)

        Returns:
            DimensionScore for trust
        """
        # Normalize confidence to 0-1 score
        if confidence_score >= threshold:
            score = min(1.0, confidence_score)
        else:
            score = max(0.0, confidence_score * 0.5)

        return DimensionScore(
            dimension=QualityDimension.TRUST,
            score=score,
            weight=self.weights.get_weight(QualityDimension.TRUST),
            metadata={"confidence": confidence_score, "threshold": threshold},
        )

    def compute_overall_score(
        self,
        dimensions: list[DimensionScore],
        training_threshold: float = 0.75,
    ) -> MultiDimensionalScore:
        """Compute overall quality score from dimensions.

        Args:
            dimensions: List of dimension scores
            training_threshold: Threshold for training eligibility (default: 0.75)

        Returns:
            MultiDimensionalScore with overall score
        """
        if not dimensions:
            overall = 0.0
        else:
            total_weight = sum(d.weight for d in dimensions)
            if total_weight == 0:
                overall = 0.0
            else:
                weighted_sum = sum(d.score * d.weight for d in dimensions)
                overall = weighted_sum / total_weight

        return MultiDimensionalScore(
            session_id="",  # Set by caller
            event_id="",  # Set by caller
            dimensions=dimensions,
            overall_score=overall,
            training_eligible=overall >= training_threshold,
        )
