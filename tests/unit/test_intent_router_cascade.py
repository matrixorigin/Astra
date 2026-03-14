"""Tests for IntentRouter cascade — Tier 0 → threshold → Tier 1 → fallback."""

import asyncio
import pytest
from unittest.mock import AsyncMock, MagicMock, patch

from core.context.intent_routing import (
    INTENT_PLANS,
    IntentRouter,
    RoutingDecision,
    RoutingResult,
    Tier1Result,
)


@pytest.fixture
def router():
    return IntentRouter(db_factory=MagicMock())


class TestTier0HighConfidenceSkip:
    @pytest.mark.asyncio
    async def test_preference_skips_tier1(self, router):
        """'记住我用vim' with regex match → 0.80 >= 0.80 threshold → skip Tier 1."""
        with patch("core.context.routing_metrics.adaptive_threshold", return_value=0.80):
            decision = await router.route("记住我用vim", history_len=3)
        assert decision.routing_result.intent == "preference"
        assert decision.routing_result.tier == 0
        assert decision.tier1_result is None
        assert decision.plan.load_tools is False
        assert decision.plan.load_memory == "profile"

    @pytest.mark.asyncio
    async def test_command_both_agree_skips_tier1(self, router):
        """'run tests' first turn → both agree → 0.95 → skip Tier 1."""
        with patch("core.context.routing_metrics.adaptive_threshold", return_value=0.85):
            decision = await router.route("run the tests", history_len=0)
        assert decision.routing_result.intent == "command"
        assert decision.routing_result.confidence == 0.95
        assert decision.tier1_result is None


class TestTier0LowConfidenceTriggersTier1:
    @pytest.mark.asyncio
    async def test_ambiguous_query_triggers_tier1(self, router):
        """'what is event sourcing?' → Tier 0 confidence 0.0 → triggers Tier 1."""
        tier1_result = Tier1Result(
            routing=RoutingResult(intent="question", confidence=0.9, tier=1, matched_by="llm"),
        )
        with (
            patch("core.context.routing_metrics.adaptive_threshold", return_value=0.85),
            patch.object(
                router._tier1, "run_parallel", new_callable=AsyncMock, return_value=tier1_result
            ),
        ):
            decision = await router.route("what is event sourcing?", history_len=3)
        assert decision.routing_result.intent == "question"
        assert decision.routing_result.tier == 1
        assert decision.tier1_result is not None

    @pytest.mark.asyncio
    async def test_high_threshold_forces_tier1(self, router):
        """Regex-only match (0.80) with threshold 0.85 → triggers Tier 1."""
        tier1_result = Tier1Result(
            routing=RoutingResult(intent="preference", confidence=0.92, tier=1, matched_by="llm"),
        )
        with (
            patch("core.context.routing_metrics.adaptive_threshold", return_value=0.85),
            patch.object(
                router._tier1, "run_parallel", new_callable=AsyncMock, return_value=tier1_result
            ),
        ):
            decision = await router.route("记住我用vim", history_len=3)
        # Tier 0 was 0.80 < 0.85 threshold, so Tier 1 ran
        assert decision.routing_result.tier == 1
        assert decision.routing_result.confidence == 0.92


class TestTier1FailureFallback:
    @pytest.mark.asyncio
    async def test_tier1_exception_falls_back(self, router):
        with (
            patch("core.context.routing_metrics.adaptive_threshold", return_value=0.85),
            patch.object(
                router._tier1,
                "run_parallel",
                new_callable=AsyncMock,
                side_effect=RuntimeError("LLM down"),
            ),
        ):
            decision = await router.route("what is this?", history_len=3)
        assert decision.routing_result.intent == "question"
        assert decision.routing_result.matched_by == "fallback"
        assert decision.plan == INTENT_PLANS["question"]

    @pytest.mark.asyncio
    async def test_tier1_low_confidence_falls_back(self, router):
        tier1_result = Tier1Result(
            routing=RoutingResult(intent="command", confidence=0.5, tier=1, matched_by="llm"),
        )
        with (
            patch("core.context.routing_metrics.adaptive_threshold", return_value=0.85),
            patch.object(
                router._tier1, "run_parallel", new_callable=AsyncMock, return_value=tier1_result
            ),
        ):
            decision = await router.route("do something", history_len=3)
        assert decision.routing_result.intent == "question"
        assert decision.routing_result.matched_by == "fallback"


class TestForceIntent:
    @pytest.mark.asyncio
    async def test_force_intent_overrides_everything(self, router):
        decision = await router.route("run tests", history_len=0, force_intent="question")
        assert decision.routing_result.intent == "question"
        assert decision.routing_result.confidence == 1.0
        assert decision.routing_result.matched_by == "forced"
        assert decision.plan == INTENT_PLANS["question"]


class TestSyncWrapper:
    def test_route_sync_works(self, router):
        with patch("core.context.routing_metrics.adaptive_threshold", return_value=0.80):
            decision = router.route_sync(query="run tests", history_len=0)
        assert decision.routing_result.intent == "command"
