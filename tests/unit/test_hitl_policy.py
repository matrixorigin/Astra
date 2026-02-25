"""Tests for HITL Policy Engine."""

from unittest.mock import MagicMock, Mock

import pytest

from core.verification.hitl_policy import (
    ActionContext,
    HITLPolicyEngine,
    PolicyDecision,
    SupervisionAction,
    SupervisionPolicy,
    SupervisionTrigger,
)


def _engine(*policies: SupervisionPolicy) -> HITLPolicyEngine:
    engine = HITLPolicyEngine(lambda: MagicMock())
    for p in policies:
        engine.add_policy(p)
    return engine


def _policy(name: str, action: SupervisionAction, **trigger_kwargs) -> SupervisionPolicy:
    return SupervisionPolicy(
        name=name,
        trigger=SupervisionTrigger(**trigger_kwargs),
        action=action,
    )


class TestNoTrigger:
    def test_no_policies_returns_none(self):
        engine = HITLPolicyEngine(lambda: MagicMock())
        ctx = ActionContext()
        decision = engine.evaluate(ctx)
        assert decision.action == SupervisionAction.NONE
        assert decision.triggered_policies == []

    def test_cost_below_threshold_no_trigger(self):
        engine = _engine(_policy("cost-gate", SupervisionAction.APPROVE_REJECT, cost_exceeds=5.0))
        decision = engine.evaluate(ActionContext(estimated_cost=4.99))
        assert decision.action == SupervisionAction.NONE

    def test_confidence_above_threshold_no_trigger(self):
        engine = _engine(_policy("conf-gate", SupervisionAction.REVIEW_AND_EDIT, confidence_below=0.6))
        decision = engine.evaluate(ActionContext(confidence=0.61))
        assert decision.action == SupervisionAction.NONE


class TestSingleTrigger:
    def test_cost_exceeds_triggers(self):
        engine = _engine(_policy("cost-gate", SupervisionAction.APPROVE_REJECT, cost_exceeds=5.0))
        decision = engine.evaluate(ActionContext(estimated_cost=5.01))
        assert decision.action == SupervisionAction.APPROVE_REJECT
        assert "cost-gate" in decision.triggered_policies

    def test_confidence_below_triggers(self):
        engine = _engine(_policy("conf", SupervisionAction.REVIEW_AND_EDIT, confidence_below=0.6))
        decision = engine.evaluate(ActionContext(confidence=0.5))
        assert decision.action == SupervisionAction.REVIEW_AND_EDIT

    def test_affects_resources_triggers(self):
        engine = _engine(_policy(
            "prod-gate", SupervisionAction.REVIEW_AND_EDIT,
            affects_resources=["production", "database"],
        ))
        decision = engine.evaluate(ActionContext(resources=["production"]))
        assert decision.action == SupervisionAction.REVIEW_AND_EDIT

    def test_affects_resources_no_overlap_no_trigger(self):
        engine = _engine(_policy(
            "prod-gate", SupervisionAction.REVIEW_AND_EDIT,
            affects_resources=["production"],
        ))
        decision = engine.evaluate(ActionContext(resources=["staging"]))
        assert decision.action == SupervisionAction.NONE

    def test_plan_depth_triggers(self):
        engine = _engine(_policy("plan-gate", SupervisionAction.APPROVE_REJECT, plan_depth_exceeds=5))
        decision = engine.evaluate(ActionContext(plan_depth=6))
        assert decision.action == SupervisionAction.APPROVE_REJECT

    def test_plan_depth_at_threshold_no_trigger(self):
        engine = _engine(_policy("plan-gate", SupervisionAction.APPROVE_REJECT, plan_depth_exceeds=5))
        decision = engine.evaluate(ActionContext(plan_depth=5))
        assert decision.action == SupervisionAction.NONE

    def test_novel_skill_triggers(self):
        engine = _engine(_policy("novel", SupervisionAction.OBSERVE_ONLY, novel_skill_use=True))
        decision = engine.evaluate(ActionContext(is_novel_skill=True, skill_name="new_tool"))
        assert decision.action == SupervisionAction.OBSERVE_ONLY

    def test_novel_skill_false_no_trigger(self):
        engine = _engine(_policy("novel", SupervisionAction.OBSERVE_ONLY, novel_skill_use=True))
        decision = engine.evaluate(ActionContext(is_novel_skill=False))
        assert decision.action == SupervisionAction.NONE

    def test_agent_escalated_triggers(self):
        engine = _engine(_policy("escalate", SupervisionAction.TAKEOVER, escalated_by_agent=True))
        decision = engine.evaluate(ActionContext(agent_escalated=True))
        assert decision.action == SupervisionAction.TAKEOVER


class TestMostRestrictiveWins:
    def test_two_policies_most_restrictive_wins(self):
        engine = _engine(
            _policy("observe", SupervisionAction.OBSERVE_ONLY, novel_skill_use=True),
            _policy("approve", SupervisionAction.APPROVE_REJECT, cost_exceeds=1.0),
        )
        decision = engine.evaluate(ActionContext(
            estimated_cost=2.0, is_novel_skill=True,
        ))
        assert decision.action == SupervisionAction.APPROVE_REJECT
        assert len(decision.triggered_policies) == 2

    def test_takeover_beats_all(self):
        engine = _engine(
            _policy("observe", SupervisionAction.OBSERVE_ONLY, novel_skill_use=True),
            _policy("approve", SupervisionAction.APPROVE_REJECT, cost_exceeds=1.0),
            _policy("takeover", SupervisionAction.TAKEOVER, escalated_by_agent=True),
        )
        decision = engine.evaluate(ActionContext(
            estimated_cost=2.0, is_novel_skill=True, agent_escalated=True,
        ))
        assert decision.action == SupervisionAction.TAKEOVER

    def test_reason_includes_all_triggered(self):
        engine = _engine(
            _policy("p1", SupervisionAction.OBSERVE_ONLY, novel_skill_use=True),
            _policy("p2", SupervisionAction.APPROVE_REJECT, cost_exceeds=1.0),
        )
        decision = engine.evaluate(ActionContext(estimated_cost=2.0, is_novel_skill=True))
        assert "p1" in decision.reason
        assert "p2" in decision.reason


class TestDisabledPolicy:
    def test_disabled_policy_not_evaluated(self):
        engine = HITLPolicyEngine(lambda: MagicMock())
        policy = SupervisionPolicy(
            name="disabled",
            trigger=SupervisionTrigger(cost_exceeds=1.0),
            action=SupervisionAction.APPROVE_REJECT,
            enabled=False,
        )
        engine.add_policy(policy)
        decision = engine.evaluate(ActionContext(estimated_cost=100.0))
        assert decision.action == SupervisionAction.NONE


class TestActionSeverity:
    def test_severity_ordering(self):
        assert SupervisionAction.NONE.severity < SupervisionAction.OBSERVE_ONLY.severity
        assert SupervisionAction.OBSERVE_ONLY.severity < SupervisionAction.APPROVE_REJECT.severity
        assert SupervisionAction.APPROVE_REJECT.severity < SupervisionAction.REVIEW_AND_EDIT.severity
        assert SupervisionAction.REVIEW_AND_EDIT.severity < SupervisionAction.TAKEOVER.severity
