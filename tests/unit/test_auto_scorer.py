"""Tests for core.evaluation.auto_scorer."""

import pytest

from core.evaluation.auto_scorer import AutoScoreResult, compute_auto_score


class TestAutoScorer:
    """Unit tests for compute_auto_score."""

    def test_perfect_response(self):
        """High-confidence firewall + reasonable length → high score + training eligible."""
        result = compute_auto_score(
            firewall_passed=True,
            firewall_confidence=0.95,
            response_tokens=200,
        )
        assert result.quality_score >= 4.0
        assert result.training_eligible is True
        assert result.no_hallucination is True
        assert result.reasonable_length is True
        assert result.within_budget is True

    def test_low_confidence_firewall(self):
        """Low firewall confidence → low score, not training eligible."""
        result = compute_auto_score(
            firewall_passed=False,
            firewall_confidence=0.2,
            response_tokens=200,
        )
        assert result.quality_score < 4.0
        assert result.training_eligible is False
        assert result.no_hallucination is False

    def test_too_short_response(self):
        """Response below minimum tokens → reasonable_length False."""
        result = compute_auto_score(
            firewall_passed=True,
            firewall_confidence=0.9,
            response_tokens=10,
        )
        assert result.reasonable_length is False
        # Still can be training eligible if firewall confidence is high enough
        # 0.9 * 5 * 0.6 + 0 * 5 * 0.2 + 1 * 5 * 0.2 = 2.7 + 0 + 1.0 = 3.7
        assert result.quality_score < 4.0

    def test_too_long_response(self):
        """Response above maximum tokens → reasonable_length False."""
        result = compute_auto_score(
            firewall_passed=True,
            firewall_confidence=0.9,
            response_tokens=3000,
        )
        assert result.reasonable_length is False

    def test_over_budget(self):
        """Actual cost exceeds estimated by >20% → within_budget False."""
        result = compute_auto_score(
            firewall_passed=True,
            firewall_confidence=0.9,
            response_tokens=200,
            actual_cost=0.15,
            estimated_cost=0.10,
        )
        assert result.within_budget is False

    def test_within_budget(self):
        """Actual cost within 20% of estimated → within_budget True."""
        result = compute_auto_score(
            firewall_passed=True,
            firewall_confidence=0.9,
            response_tokens=200,
            actual_cost=0.11,
            estimated_cost=0.10,
        )
        assert result.within_budget is True

    def test_no_cost_info(self):
        """Missing cost info → within_budget defaults to True."""
        result = compute_auto_score(
            firewall_passed=True,
            firewall_confidence=0.9,
            response_tokens=200,
        )
        assert result.within_budget is True

    def test_score_clamped_to_5(self):
        """Score never exceeds 5.0."""
        result = compute_auto_score(
            firewall_passed=True,
            firewall_confidence=1.0,
            response_tokens=200,
        )
        assert result.quality_score <= 5.0

    def test_score_clamped_to_0(self):
        """Score never goes below 0.0."""
        result = compute_auto_score(
            firewall_passed=False,
            firewall_confidence=0.0,
            response_tokens=5,
        )
        assert result.quality_score >= 0.0

    def test_zero_estimated_cost(self):
        """Zero estimated cost → skip budget check (avoid division by zero)."""
        result = compute_auto_score(
            firewall_passed=True,
            firewall_confidence=0.9,
            response_tokens=200,
            actual_cost=0.05,
            estimated_cost=0.0,
        )
        assert result.within_budget is True
