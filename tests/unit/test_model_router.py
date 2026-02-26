"""Tests for self-improving model routing."""

from unittest.mock import Mock

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
        db = _mock_db()
        router = ModelRouter(lambda: db)

        assert router.classify_complexity("deploy", "Deploy to production") == TaskComplexity.CRITICAL
        assert router.classify_complexity("security", "Security review") == TaskComplexity.CRITICAL

    def test_classify_complexity_complex(self):
        db = _mock_db()
        router = ModelRouter(lambda: db)

        assert router.classify_complexity("refactor", "Refactor auth module") == TaskComplexity.COMPLEX
        assert router.classify_complexity("design", "Design new API") == TaskComplexity.COMPLEX

    def test_classify_complexity_medium(self):
        db = _mock_db()
        router = ModelRouter(lambda: db)

        assert router.classify_complexity("edit", "Fix typo") == TaskComplexity.MEDIUM
        assert router.classify_complexity("review", "Review PR") == TaskComplexity.MEDIUM

    def test_classify_complexity_simple(self):
        db = _mock_db()
        router = ModelRouter(lambda: db)

        assert router.classify_complexity("status", "What's the CI status?") == TaskComplexity.SIMPLE
        assert router.classify_complexity("info", "Tell me about X") == TaskComplexity.SIMPLE

    def test_route_critical(self):
        db = _mock_db()
        # _get_efficiency_ranking uses ORM query chain → returns empty
        db.query.return_value.filter.return_value.group_by.return_value.order_by.return_value.all.return_value = []
        # _estimate_cost uses raw SQL db.execute
        db.execute.return_value = Mock(fetchone=Mock(return_value=(0.03,)))

        router = ModelRouter(lambda: db)
        decision = router.route(
            task_type="deploy",
            query="Deploy to production",
            available_models=["gpt-4", "gpt-3.5"],
            scope_id="user-1",
        )

        assert decision.model == "gpt-4"
        assert decision.complexity == TaskComplexity.CRITICAL
        assert decision.estimated_cost == 0.03

    def test_route_simple(self):
        db = _mock_db()
        db.query.return_value.filter.return_value.group_by.return_value.order_by.return_value.all.return_value = []
        db.execute.return_value = Mock(fetchone=Mock(return_value=(0.001,)))

        router = ModelRouter(lambda: db)
        decision = router.route(
            task_type="status",
            query="What's the status?",
            available_models=["gpt-4", "gpt-3.5"],
            scope_id="user-1",
        )

        assert decision.model == "gpt-3.5"
        assert decision.complexity == TaskComplexity.SIMPLE
        assert decision.estimated_cost == 0.001

    def test_record_quality(self):
        db = _mock_db()
        router = ModelRouter(lambda: db)

        router.record_quality(
            task_type="code_review",
            model="gpt-4",
            quality_score=4.5,
            cost=0.03,
        )

        db.add.assert_called_once()
        db.commit.assert_called_once()

    def test_get_efficiency_ranking(self):
        db = _mock_db()
        # ORM: query(...).filter(...).group_by(...).order_by(...).all()
        row1 = ("gpt-4", 4.5, 0.03, 150.0)
        row2 = ("gpt-3.5", 4.0, 0.001, 4000.0)
        db.query.return_value.filter.return_value.group_by.return_value.order_by.return_value.all.return_value = [row1, row2]

        router = ModelRouter(lambda: db)
        efficiency = router._get_efficiency_ranking("code_review")

        assert efficiency["gpt-4"] == 150.0
        assert efficiency["gpt-3.5"] == 4000.0

    def test_estimate_cost_from_db(self):
        db = _mock_db()
        db.execute.return_value = Mock(fetchone=Mock(return_value=(0.025,)))

        router = ModelRouter(lambda: db)
        cost = router._estimate_cost("gpt-4", "code_review")

        assert cost == 0.025

    def test_estimate_cost_default(self):
        db = _mock_db()
        db.execute.return_value = Mock(fetchone=Mock(return_value=None))

        router = ModelRouter(lambda: db)
        cost = router._estimate_cost("gpt-4", "unknown_task")

        assert cost == 0.03  # Default for gpt-4
