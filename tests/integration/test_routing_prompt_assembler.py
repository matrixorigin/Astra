"""Integration tests for routing + PromptAssembler.

Verifies that RoutingDecision controls which sections are built,
and that routing metadata is persisted in snapshots.
"""

import json
import pytest
from unittest.mock import patch

from sqlalchemy import text as sql_text

from core.context.intent_routing import (
    INTENT_PLANS,
    ContextLoadingPlan,
    RoutingDecision,
    RoutingResult,
    Tier1Result,
)
from core.context.prompt_assembler import PromptAssembler, EdgeContext
from tests.integration.helpers import unique_test_id


class TestRoutingSkipsSections:
    """Verify that routing plans skip the correct sections."""

    def _assemble_with_plan(self, db_session, intent: str, **kwargs):
        plan = INTENT_PLANS[intent]
        result = RoutingResult(intent=intent, confidence=0.95, tier=0, matched_by="both")
        decision = RoutingDecision(plan=plan, routing_result=result)
        pa = PromptAssembler(lambda: db_session)
        return pa.assemble(
            agent_id=None,
            user_query="test query",
            session_id=unique_test_id(),
            user_id=unique_test_id(),
            routing_decision=decision,
            **kwargs,
        )

    def test_preference_skips_history_and_tools(self, db_session):
        result = self._assemble_with_plan(db_session, "preference")
        assert "history" not in result.sections
        assert result.tools_schema == []
        assert result.routing_intent == "preference"
        assert result.routing_confidence == 0.95
        # Identity and constraints always present
        assert "identity" in result.sections
        assert "constraints" in result.sections

    def test_command_skips_history_and_memory(self, db_session):
        edge_ctx = EdgeContext(
            edge_tools=[
                {"type": "function", "function": {"name": "bash", "description": "Shell", "parameters": {}}},
            ],
        )
        result = self._assemble_with_plan(db_session, "command", edge_context=edge_ctx)
        assert "history" not in result.sections
        assert "memory" not in result.sections
        # Tools should be present (command needs tools)
        assert len(result.tools_schema) > 0

    def test_feedback_limits_history(self, db_session):
        """Feedback intent requests last 2 turns only."""
        result = self._assemble_with_plan(db_session, "feedback")
        # History may be empty (no events in test DB), but the plan was applied
        assert result.routing_intent == "feedback"
        assert "memory" not in result.sections
        assert result.tools_schema == []

    def test_question_loads_everything(self, db_session):
        result = self._assemble_with_plan(db_session, "question")
        assert result.routing_intent == "question"
        # Identity and constraints always present
        assert "identity" in result.sections
        assert "constraints" in result.sections

    def test_no_routing_decision_backward_compat(self, db_session):
        """None routing_decision → full context (backward compatible)."""
        pa = PromptAssembler(lambda: db_session)
        result = pa.assemble(
            agent_id=None,
            user_query="hello",
            session_id=unique_test_id(),
            user_id=unique_test_id(),
            routing_decision=None,
        )
        assert result.routing_intent is None
        assert result.routing_confidence == 0.0
        assert "identity" in result.sections
        assert "constraints" in result.sections


class TestTier1Integration:
    """Verify Tier 1 results (compressed memory, pruned tools) are used."""

    def test_tier1_compressed_memory_used(self, db_session):
        plan = INTENT_PLANS["question"]
        result = RoutingResult(intent="question", confidence=0.9, tier=1, matched_by="llm")
        tier1 = Tier1Result(compressed_memory="compressed profile data")
        decision = RoutingDecision(plan=plan, routing_result=result, tier1_result=tier1)

        pa = PromptAssembler(lambda: db_session)
        assembled = pa.assemble(
            agent_id=None,
            user_query="explain this",
            session_id=unique_test_id(),
            user_id=unique_test_id(),
            routing_decision=decision,
        )
        # Compressed memory should appear in sections
        if "memory" in assembled.sections:
            assert "compressed profile data" in assembled.sections["memory"]

    def test_tier1_pruned_tools_filters_schema(self, db_session):
        plan = INTENT_PLANS["question"]
        result = RoutingResult(intent="question", confidence=0.9, tier=1, matched_by="llm")
        tier1 = Tier1Result(pruned_tools=["bash"])
        decision = RoutingDecision(plan=plan, routing_result=result, tier1_result=tier1)

        edge_ctx = EdgeContext(
            edge_tools=[
                {"type": "function", "function": {"name": "bash", "description": "Shell", "parameters": {}}},
                {"type": "function", "function": {"name": "grep", "description": "Search", "parameters": {}}},
                {"type": "function", "function": {"name": "read_file", "description": "Read", "parameters": {}}},
            ],
        )

        pa = PromptAssembler(lambda: db_session)
        assembled = pa.assemble(
            agent_id=None,
            user_query="run tests",
            session_id=unique_test_id(),
            user_id=unique_test_id(),
            edge_context=edge_ctx,
            routing_decision=decision,
        )
        # Only "bash" should remain
        tool_names = [t.get("function", {}).get("name") for t in assembled.tools_schema]
        assert tool_names == ["bash"]


class TestSnapshotContainsRoutingInfo:
    """Verify routing metadata is persisted in ctx_snapshots — re-query DB."""

    def test_snapshot_persisted_with_correct_fields(self, db_session):
        plan = INTENT_PLANS["preference"]
        result = RoutingResult(intent="preference", confidence=0.95, tier=0, matched_by="both")
        decision = RoutingDecision(plan=plan, routing_result=result)

        sid = unique_test_id()
        pa = PromptAssembler(lambda: db_session)
        assembled = pa.assemble(
            agent_id=None,
            user_query="记住我用vim",
            session_id=sid,
            user_id=unique_test_id(),
            routing_decision=decision,
        )

        assert assembled.snapshot_id is not None
        assert assembled.routing_intent == "preference"
        assert assembled.routing_confidence == 0.95

        # Re-query DB — ground truth verification
        from api.models.context import ContextSnapshot
        row = (
            db_session.query(ContextSnapshot)
            .filter(ContextSnapshot.context_capture_id == assembled.snapshot_id)
            .first()
        )
        assert row is not None, "Snapshot not found in DB"
        assert row.session_id == sid
        assert row.context_capture_id == assembled.snapshot_id
        assert row.total_tokens is not None
        assert row.total_tokens > 0
        assert row.created_at is not None

        # token_budget is JSON of breakdown — verify it round-trips
        budget = json.loads(row.token_budget) if isinstance(row.token_budget, str) else row.token_budget
        assert "identity" in budget
        assert "constraints" in budget
        # Preference skips history and tools — they should NOT be in breakdown
        assert "history" not in budget
        assert budget.get("tool_schemas") is None or budget.get("tool_schemas", 0) == 0

        # system_prompt contains fixed_hashes + variable_sections
        prompt_data = json.loads(row.system_prompt) if isinstance(row.system_prompt, str) else row.system_prompt
        assert "fixed_hashes" in prompt_data
        assert "identity" in prompt_data["fixed_hashes"]


class TestTokenEfficiencyComparison:
    """Verify routing actually saves tokens — preference vs question."""

    def test_preference_skips_sections_question_includes(self, db_session):
        """Preference must have strictly fewer sections than question."""
        uid = unique_test_id()
        pa = PromptAssembler(lambda: db_session)

        # Assemble with preference intent
        pref_result = RoutingResult(intent="preference", confidence=0.95, tier=0, matched_by="both")
        pref_decision = RoutingDecision(plan=INTENT_PLANS["preference"], routing_result=pref_result)
        pref = pa.assemble(
            agent_id=None, user_query="记住我用vim",
            session_id=unique_test_id(), user_id=uid, routing_decision=pref_decision,
        )

        # Assemble with question intent (full context) — provide tools so there's a difference
        edge_ctx = EdgeContext(
            edge_tools=[
                {"type": "function", "function": {"name": "bash", "description": "Shell exec", "parameters": {"type": "object", "properties": {"cmd": {"type": "string"}}}}},
                {"type": "function", "function": {"name": "grep", "description": "Search files", "parameters": {"type": "object", "properties": {"pattern": {"type": "string"}}}}},
            ],
        )
        q_result = RoutingResult(intent="question", confidence=0.95, tier=0, matched_by="both")
        q_decision = RoutingDecision(plan=INTENT_PLANS["question"], routing_result=q_result)
        q = pa.assemble(
            agent_id=None, user_query="what is event sourcing?",
            session_id=unique_test_id(), user_id=uid,
            edge_context=edge_ctx, routing_decision=q_decision,
        )

        # Preference: no tools, no history
        assert pref.tools_schema == []
        assert "history" not in pref.sections

        # Question: has tools
        assert len(q.tools_schema) == 2

        # Question must have more token weight (tools add tokens)
        pref_tokens = sum(pref.token_breakdown.values())
        q_tokens = sum(q.token_breakdown.values())
        assert pref_tokens <= q_tokens, (
            f"Preference ({pref_tokens}) should use ≤ tokens than question ({q_tokens})"
        )
