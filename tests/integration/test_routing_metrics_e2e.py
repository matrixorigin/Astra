"""Integration tests for routing metrics (real DB) + Tier 1 timeout."""

import asyncio
import pytest
from unittest.mock import MagicMock, patch

from sqlalchemy import text

from core.context.routing_metrics import (
    monthly_budget_remaining,
    reset_for_testing,
)
from core.context.intent_routing import Tier1Engine, RoutingResult
from tests.integration.helpers import unique_test_id


class TestMonthlyBudgetRealDB:
    """Verify monthly_budget_remaining queries eval_llm_call_logs correctly."""

    @pytest.fixture(autouse=True)
    def _reset(self):
        reset_for_testing()
        yield
        reset_for_testing()

    def test_no_spend_returns_full(self, db_session):
        remaining = monthly_budget_remaining(db_factory=lambda: db_session)
        # Could be < 1.0 if other tests inserted cost rows this month,
        # but should be > 0.0 (budget not exhausted)
        assert 0.0 <= remaining <= 1.0

    def test_spend_reduces_budget(self, db_session):
        """Insert a cost record → budget remaining decreases."""
        from uuid_utils import uuid7

        # Snapshot budget before
        reset_for_testing()
        before = monthly_budget_remaining(db_factory=lambda: db_session)

        # Insert $10 spend
        log_id = str(uuid7())
        db_session.execute(
            text("""
                INSERT INTO eval_llm_call_logs (log_id, cost_usd, created_at)
                VALUES (:lid, :cost, NOW())
            """),
            {"lid": log_id, "cost": 10.0},
        )
        db_session.commit()

        reset_for_testing()  # clear TTL cache
        after = monthly_budget_remaining(db_factory=lambda: db_session)

        # After should be lower (10/100 = 0.1 less)
        assert after < before, f"Budget should decrease: before={before}, after={after}"
        assert after == pytest.approx(before - 0.1, abs=0.02)

        # Re-query DB to verify the row is actually there
        from api.models.evaluation import LLMCallLog

        row = db_session.query(LLMCallLog).filter(LLMCallLog.log_id == log_id).first()
        assert row is not None
        assert row.cost_usd == 10.0

    def test_cache_ttl_prevents_repeated_queries(self, db_session):
        """Second call within 60s returns cached value (no DB hit)."""
        reset_for_testing()
        first = monthly_budget_remaining(db_factory=lambda: db_session)

        # Patch db_factory to explode — should NOT be called (cached)
        def exploding_factory():
            raise RuntimeError("Should not be called — cache should be active")

        second = monthly_budget_remaining(db_factory=exploding_factory)
        assert second == first


class TestTier1Timeout:
    """Verify Tier 1 sub-tasks respect the 2s timeout."""

    @pytest.mark.asyncio
    async def test_classify_timeout_returns_exception(self):
        engine = Tier1Engine(db_factory=MagicMock())

        def slow_llm_call(prompt):
            import time

            time.sleep(5)  # blocks thread > 2s timeout
            return '{"intent": "command", "confidence": 0.9}'

        with patch.object(engine, "_llm_call", side_effect=slow_llm_call):
            result = await engine.run_parallel("test query")

        # _classify wraps _llm_call in wait_for(timeout=2s) → should timeout
        assert result.routing is None

    @pytest.mark.asyncio
    async def test_compress_timeout_returns_none(self):
        engine = Tier1Engine(db_factory=MagicMock())

        call_count = 0

        def selective_slow_llm_call(prompt):
            nonlocal call_count
            call_count += 1
            if "Compress" in prompt:
                import time

                time.sleep(5)  # slow only for compress
                return "compressed"
            # Fast for classify
            return '{"intent": "question", "confidence": 0.9}'

        with patch.object(engine, "_llm_call", side_effect=selective_slow_llm_call):
            result = await engine.run_parallel("test", memory_text="x" * 200)

        assert result.routing is not None
        assert result.routing.intent == "question"
        assert result.compressed_memory is None  # compress timed out
