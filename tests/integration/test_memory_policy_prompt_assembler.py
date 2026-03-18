"""Integration tests for memory policy wiring into PromptAssembler and IntentRouter."""

from __future__ import annotations

import pytest

from core.context.intent_routing import IntentRouter
from core.context.prompt_assembler import PromptAssembler
from core.memory.policy import MemoryContextMode
from tests.integration.helpers import unique_test_id


class TestIntentRouterMemoryPolicy:
    @pytest.mark.asyncio
    async def test_router_populates_memory_policy(self, db_session):
        router = IntentRouter(db_factory=lambda: db_session)
        decision = await router.route("what do you remember about pytest config?", history_len=0)

        assert decision.memory_policy is not None
        assert decision.memory_policy.tool_hint.tool_name == "memory_retrieve"
        assert decision.memory_policy.context_plan.mode == MemoryContextMode.RETRIEVE


class TestPromptAssemblerMemoryGuidance:
    def test_constraints_include_memory_guidance(self, db_session):
        router = IntentRouter(db_factory=lambda: db_session)
        decision = router.route_sync(query="remember that I use vim", history_len=0)

        assembled = PromptAssembler(lambda: db_session).assemble(
            agent_id=None,
            user_query="remember that I use vim",
            session_id=unique_test_id(),
            user_id=unique_test_id(),
            routing_decision=decision,
        )

        constraints = assembled.sections["constraints"]
        assert "Memory is part of system context" in constraints
        assert "`memory_store`" in constraints

    def test_memory_stats_include_policy(self, db_session):
        router = IntentRouter(db_factory=lambda: db_session)
        decision = router.route_sync(query="what do you know about matrixone?", history_len=0)

        assembled = PromptAssembler(lambda: db_session).assemble(
            agent_id=None,
            user_query="what do you know about matrixone?",
            session_id=unique_test_id(),
            user_id=unique_test_id(),
            routing_decision=decision,
            explain=True,
        )

        assert assembled.memory_stats is not None
        assert assembled.memory_stats["policy"]["mode"] == "search"
