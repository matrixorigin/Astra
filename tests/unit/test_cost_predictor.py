"""Unit tests for Cost-Aware Branching."""

from unittest.mock import MagicMock

import pytest

from core.sandbox.cost_predictor import BranchCostPredictor, CostEstimate


@pytest.fixture
def db():
    return MagicMock()


@pytest.fixture
def router():
    r = MagicMock()
    # gpt-4o: $5/1M tokens, gpt-4o-mini: $0.15/1M tokens
    def estimate(model, tokens):
        prices = {"gpt-4o": 0.005, "gpt-4o-mini": 0.00015, "claude-haiku-3.5": 0.001}
        return round(tokens * prices.get(model, 0.003) / 1000, 6)
    r.estimate_cost.side_effect = estimate
    return r


@pytest.fixture
def predictor(db, router):
    return BranchCostPredictor(db, router)


class TestReplayEstimate:
    def test_basic_estimate(self, predictor):
        est = predictor.estimate_replay(
            session_count=10, model="gpt-4o", avg_tokens_override=3000,
        )
        assert est.operation == "replay"
        assert est.model == "gpt-4o"
        assert est.estimated_tokens > 0
        assert est.estimated_cost > 0
        assert not est.exceeds_budget

    def test_exceeds_budget(self, predictor):
        est = predictor.estimate_replay(
            session_count=100, model="gpt-4o",
            budget_remaining=1.0, avg_tokens_override=3000,
        )
        assert est.exceeds_budget is True
        assert len(est.alternatives) > 0

    def test_within_budget(self, predictor):
        est = predictor.estimate_replay(
            session_count=1, model="gpt-4o-mini",
            budget_remaining=100.0, avg_tokens_override=100,
        )
        assert est.exceeds_budget is False
        assert est.alternatives == []

    def test_alternatives_include_cheaper_model(self, predictor):
        est = predictor.estimate_replay(
            session_count=50, model="gpt-4o",
            budget_remaining=0.5, avg_tokens_override=3000,
        )
        strategies = [a["strategy"] for a in est.alternatives]
        assert "cheaper_model" in strategies


class TestBranchEstimate:
    def test_create_is_free(self, predictor):
        est = predictor.estimate_branch("create", "gpt-4o")
        assert est.estimated_cost == 0.0
        assert est.estimated_tokens == 0

    def test_delete_is_free(self, predictor):
        est = predictor.estimate_branch("delete", "gpt-4o")
        assert est.estimated_cost == 0.0

    def test_merge_has_cost(self, predictor):
        est = predictor.estimate_branch(
            "merge", "gpt-4o", session_count=100,
        )
        # 10% conflict rate → some cost
        assert est.estimated_cost > 0

    def test_merge_exceeds_budget(self, predictor):
        # Force high token count via historical avg mock
        predictor._get_historical_avg_tokens = lambda m=None: 5000
        predictor._get_historical_avg_turns = lambda: 10
        est = predictor.estimate_branch(
            "merge", "gpt-4o", session_count=1000,
            budget_remaining=0.001,
        )
        assert est.exceeds_budget is True


class TestFallbackPricing:
    def test_no_router_uses_fallback(self, db):
        predictor = BranchCostPredictor(db, model_router=None)
        est = predictor.estimate_replay(
            session_count=1, model="gpt-4o", avg_tokens_override=1000,
        )
        assert est.estimated_cost > 0

    def test_unknown_model_fallback(self, db):
        predictor = BranchCostPredictor(db, model_router=None)
        est = predictor.estimate_replay(
            session_count=1, model="unknown-model", avg_tokens_override=1000,
        )
        assert est.estimated_cost > 0


class TestHistoricalData:
    def test_db_query_failure_uses_default(self, db):
        db.execute.side_effect = RuntimeError("DB down")
        predictor = BranchCostPredictor(db)
        avg = predictor._get_historical_avg_tokens()
        assert avg == 3000  # default

    def test_db_returns_none_uses_default(self, db):
        db.execute.return_value.scalar.return_value = None
        predictor = BranchCostPredictor(db)
        avg = predictor._get_historical_avg_tokens()
        assert avg == 3000
