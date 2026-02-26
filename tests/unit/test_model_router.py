"""Tests for self-improving model routing."""

from unittest.mock import Mock, patch

import pytest

from core.agents.routing import ModelRouter, RouteDecision, TaskComplexity


def _mock_db():
    return Mock()


class TestTaskComplexity:
    def test_complexity_values(self):
        assert TaskComplexity.SIMPLE.value == "simple"
        assert TaskComplexity.MEDIUM.value == "medium"
        assert TaskComplexity.COMPLEX.value == "complex"
        assert TaskComplexity.CRITICAL.value == "critical"


class TestRouteDecision:
    def test_decision_creation(self):
        decision = RouteDecision(
            model="gpt-4",
            complexity=TaskComplexity.COMPLEX,
            reason="Complex task",
            estimated_cost=0.03,
        )
        assert decision.model == "gpt-4"
        assert decision.complexity == TaskComplexity.COMPLEX
        assert decision.estimated_cost == 0.03


class TestModelRouter:
    def test_classify_complexity_critical(self):
        router = ModelRouter(lambda: _mock_db())
        assert router.classify_complexity("deploy", "Deploy to production") == TaskComplexity.CRITICAL
        assert router.classify_complexity("security", "Security review") == TaskComplexity.CRITICAL

    def test_classify_complexity_complex(self):
        router = ModelRouter(lambda: _mock_db())
        assert router.classify_complexity("refactor", "Refactor auth module") == TaskComplexity.COMPLEX
        assert router.classify_complexity("design", "Design new API") == TaskComplexity.COMPLEX

    def test_classify_complexity_medium(self):
        router = ModelRouter(lambda: _mock_db())
        assert router.classify_complexity("edit", "Fix typo") == TaskComplexity.MEDIUM
        assert router.classify_complexity("review", "Review PR") == TaskComplexity.MEDIUM

    def test_classify_complexity_simple(self):
        router = ModelRouter(lambda: _mock_db())
        assert router.classify_complexity("status", "What's the CI status?") == TaskComplexity.SIMPLE
        assert router.classify_complexity("info", "Tell me about X") == TaskComplexity.SIMPLE

    def test_route_critical_selects_best_model(self):
        """Critical tasks always pick the most capable (first) model."""
        router = ModelRouter(lambda: _mock_db())

        with patch.object(router, "_get_efficiency_ranking", return_value={}), \
             patch.object(router, "_estimate_cost", return_value=0.03):
            decision = router.route("deploy", "Deploy to production",
                                    available_models=["gpt-4", "gpt-3.5"], scope_id="u1")

        assert decision.model == "gpt-4"
        assert decision.complexity == TaskComplexity.CRITICAL
        assert decision.estimated_cost == 0.03

    def test_route_simple_selects_cheapest_model(self):
        """Simple tasks pick the cheapest (last) model."""
        router = ModelRouter(lambda: _mock_db())

        with patch.object(router, "_get_efficiency_ranking", return_value={}), \
             patch.object(router, "_estimate_cost", return_value=0.001):
            decision = router.route("status", "What's the status?",
                                    available_models=["gpt-4", "gpt-3.5"], scope_id="u1")

        assert decision.model == "gpt-3.5"
        assert decision.complexity == TaskComplexity.SIMPLE
        assert decision.estimated_cost == 0.001

    def test_record_quality_persists_via_orm(self):
        db = _mock_db()
        router = ModelRouter(lambda: db)

        router.record_quality(task_type="code_review", model="gpt-4",
                              quality_score=4.5, cost=0.03)

        db.add.assert_called_once()
        db.commit.assert_called_once()

    def test_get_efficiency_ranking_returns_model_scores(self):
        """Verify efficiency dict is built from ORM query rows."""
        db = _mock_db()
        # ORM chain: query().filter().group_by().order_by().all()
        db.query.return_value.filter.return_value \
            .group_by.return_value.order_by.return_value \
            .all.return_value = [
                ("gpt-4", 4.5, 0.03, 150.0),
                ("gpt-3.5", 4.0, 0.001, 4000.0),
            ]

        router = ModelRouter(lambda: db)
        efficiency = router._get_efficiency_ranking("code_review")

        assert efficiency == {"gpt-4": 150.0, "gpt-3.5": 4000.0}

    def test_estimate_cost_from_db(self):
        db = _mock_db()
        db.query.return_value.filter.return_value.scalar.return_value = 0.025

        router = ModelRouter(lambda: db)
        assert router._estimate_cost("gpt-4", "code_review") == 0.025

    def test_estimate_cost_falls_back_to_default(self):
        db = _mock_db()
        db.query.return_value.filter.return_value.scalar.return_value = None

        router = ModelRouter(lambda: db)
        assert router._estimate_cost("gpt-4", "unknown_task") == 0.03
