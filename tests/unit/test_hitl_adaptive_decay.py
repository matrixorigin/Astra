"""Tests for Adaptive Supervision Decay in HITLPolicyEngine."""

import pytest
from unittest.mock import MagicMock

from core.verification.hitl_policy import (
    ActionContext,
    HITLPolicyEngine,
    SupervisionAction,
    SupervisionPolicy,
    SupervisionTrigger,
)


def _engine(decay_threshold: int = 5) -> HITLPolicyEngine:
    engine = HITLPolicyEngine(lambda: MagicMock(), decay_threshold=decay_threshold)
    engine.add_policy(SupervisionPolicy(
        name="novel-gate",
        trigger=SupervisionTrigger(novel_skill_use=True),
        action=SupervisionAction.APPROVE_REJECT,
    ))
    return engine


class TestDecayBasics:
    def test_no_streak_no_decay(self):
        engine = _engine()
        ctx = ActionContext(is_novel_skill=True, skill_name="tool_a")
        assert engine.evaluate(ctx).action == SupervisionAction.APPROVE_REJECT

    def test_streak_below_threshold_no_decay(self):
        engine = _engine(decay_threshold=5)
        for _ in range(4):
            engine.record_outcome("tool_a", success=True)
        ctx = ActionContext(is_novel_skill=True, skill_name="tool_a")
        assert engine.evaluate(ctx).action == SupervisionAction.APPROVE_REJECT

    def test_streak_at_threshold_triggers_decay(self):
        engine = _engine(decay_threshold=5)
        for _ in range(5):
            engine.record_outcome("tool_a", success=True)
        ctx = ActionContext(is_novel_skill=True, skill_name="tool_a")
        decision = engine.evaluate(ctx)
        # APPROVE_REJECT (severity 2) → OBSERVE_ONLY (severity 1)
        assert decision.action == SupervisionAction.OBSERVE_ONLY
        assert "decay" in decision.reason

    def test_failure_resets_streak(self):
        engine = _engine(decay_threshold=3)
        for _ in range(3):
            engine.record_outcome("tool_a", success=True)
        engine.record_outcome("tool_a", success=False)
        ctx = ActionContext(is_novel_skill=True, skill_name="tool_a")
        assert engine.evaluate(ctx).action == SupervisionAction.APPROVE_REJECT

    def test_decay_observe_only_to_none(self):
        """OBSERVE_ONLY decays to NONE."""
        engine = HITLPolicyEngine(lambda: MagicMock(), decay_threshold=2)
        engine.add_policy(SupervisionPolicy(
            name="obs",
            trigger=SupervisionTrigger(novel_skill_use=True),
            action=SupervisionAction.OBSERVE_ONLY,
        ))
        for _ in range(2):
            engine.record_outcome("x", success=True)
        ctx = ActionContext(is_novel_skill=True, skill_name="x")
        assert engine.evaluate(ctx).action == SupervisionAction.NONE

    def test_different_skills_independent_streaks(self):
        engine = _engine(decay_threshold=3)
        for _ in range(3):
            engine.record_outcome("tool_a", success=True)
        # tool_b has no streak
        ctx_b = ActionContext(is_novel_skill=True, skill_name="tool_b")
        assert engine.evaluate(ctx_b).action == SupervisionAction.APPROVE_REJECT
        # tool_a has streak
        ctx_a = ActionContext(is_novel_skill=True, skill_name="tool_a")
        assert engine.evaluate(ctx_a).action == SupervisionAction.OBSERVE_ONLY


class TestDecayEdgeCases:
    def test_no_skill_name_uses_empty_string(self):
        engine = _engine(decay_threshold=2)
        for _ in range(2):
            engine.record_outcome("", success=True)
        ctx = ActionContext(is_novel_skill=True, skill_name=None)
        # skill_name=None → "" in evaluate, matches streak for ""
        assert engine.evaluate(ctx).action == SupervisionAction.OBSERVE_ONLY

    def test_decay_threshold_one(self):
        engine = _engine(decay_threshold=1)
        engine.record_outcome("t", success=True)
        ctx = ActionContext(is_novel_skill=True, skill_name="t")
        assert engine.evaluate(ctx).action == SupervisionAction.OBSERVE_ONLY
