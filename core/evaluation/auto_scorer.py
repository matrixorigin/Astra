"""Auto-scoring for agent responses — fills quality_score and training_eligible.

Ref: evaluation-and-evolution.md §1 "Early Auto-Metrics"

Computes lightweight rule-based metrics immediately after each response,
without waiting for user feedback. Populates quality_score (0-5) and
training_eligible on the conversation event.
"""

from __future__ import annotations

from dataclasses import dataclass

_MIN_RESPONSE_TOKENS = 50
_MAX_RESPONSE_TOKENS = 2000
_COST_OVERRUN_FACTOR = 1.2
_TRAINING_QUALITY_THRESHOLD = 4.0


@dataclass
class AutoScoreResult:
    """Individual metric results and final score."""

    no_hallucination: bool
    reasonable_length: bool
    within_budget: bool
    quality_score: float  # 0-5
    training_eligible: bool


def compute_auto_score(
    *,
    firewall_passed: bool,
    firewall_confidence: float,
    response_tokens: int,
    actual_cost: float | None = None,
    estimated_cost: float | None = None,
) -> AutoScoreResult:
    """Compute auto-metrics from post-response signals.

    Args:
        firewall_passed: True if firewall.safe_to_deliver was True
        firewall_confidence: Firewall confidence_score (0-1)
        response_tokens: Number of tokens in the response
        actual_cost: Actual LLM call cost (optional)
        estimated_cost: Pre-estimated cost (optional)

    Returns:
        AutoScoreResult with quality_score (0-5) and training_eligible
    """
    no_hallucination = firewall_passed
    reasonable_length = _MIN_RESPONSE_TOKENS <= response_tokens <= _MAX_RESPONSE_TOKENS
    within_budget = True
    if actual_cost is not None and estimated_cost is not None and estimated_cost > 0:
        within_budget = actual_cost <= estimated_cost * _COST_OVERRUN_FACTOR

    # Weighted score: hallucination dominates (firewall confidence is 0-1, scale to 0-5)
    # Weights: hallucination 0.6, length 0.2, budget 0.2
    score = (
        firewall_confidence * 5.0 * 0.6
        + (1.0 if reasonable_length else 0.0) * 5.0 * 0.2
        + (1.0 if within_budget else 0.0) * 5.0 * 0.2
    )
    # Clamp
    score = max(0.0, min(5.0, round(score, 2)))

    return AutoScoreResult(
        no_hallucination=no_hallucination,
        reasonable_length=reasonable_length,
        within_budget=within_budget,
        quality_score=score,
        training_eligible=score >= _TRAINING_QUALITY_THRESHOLD,
    )
