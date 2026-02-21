"""Tests for HITL policy wiring into ChatLoop."""

import json
from unittest.mock import MagicMock, patch

import pytest

from core.verification.hitl_policy import (
    ActionContext,
    HITLPolicyEngine,
    PolicyDecision,
    SupervisionAction,
    SupervisionPolicy,
    SupervisionTrigger,
)


class FakeLLMConfig(dict):
    """Dict that also works as llm_client.config."""
    pass


def _make_loop(*, hitl_policy=None):
    """Build a ChatLoop with all deps mocked."""
    from core.agent.chat_loop import ChatLoop

    mock_selector = MagicMock()
    mock_executor = MagicMock()
    mock_llm = MagicMock()
    mock_llm.config = {"model": "test"}
    mock_logger = MagicMock()
    mock_logger.create_llm_response.return_value = MagicMock(event_id="e1")
    mock_ctx = MagicMock()
    mock_fw = MagicMock()

    loop = ChatLoop(
        selector=mock_selector,
        executor=mock_executor,
        llm_client=mock_llm,
        event_logger=mock_logger,
        context_manager=mock_ctx,
        firewall=mock_fw,
        hitl_policy=hitl_policy,
    )
    return loop


class TestEvaluateHitl:
    def test_no_policy_engine_allows(self):
        loop = _make_loop(hitl_policy=None)
        ok, msg = loop._evaluate_hitl("any_tool", {})
        assert ok is True
        assert msg is None

    def test_none_action_allows(self):
        engine = HITLPolicyEngine(db=None)
        loop = _make_loop(hitl_policy=engine)
        ok, msg = loop._evaluate_hitl("safe_tool", {})
        assert ok is True
        assert msg is None

    def test_observe_only_allows(self):
        engine = HITLPolicyEngine(db=None)
        engine.add_policy(SupervisionPolicy(
            name="obs",
            trigger=SupervisionTrigger(novel_skill_use=True),
            action=SupervisionAction.OBSERVE_ONLY,
        ))
        loop = _make_loop(hitl_policy=engine)
        ok, msg = loop._evaluate_hitl("new_tool", {"x": 1})
        # observe_only → allowed
        assert ok is True

    def test_approve_reject_blocks(self):
        engine = HITLPolicyEngine(db=None)
        engine.add_policy(SupervisionPolicy(
            name="always-block",
            trigger=SupervisionTrigger(confidence_below=2.0),  # confidence defaults 1.0 < 2.0
            action=SupervisionAction.APPROVE_REJECT,
        ))
        loop = _make_loop(hitl_policy=engine)
        ok, msg = loop._evaluate_hitl("risky_tool", {})
        assert ok is False
        data = json.loads(msg)
        assert data["hitl_action"] == "approve_reject"
        assert "always-block" in data["triggered_policies"]

    def test_takeover_blocks(self):
        engine = HITLPolicyEngine(db=None)
        engine.add_policy(SupervisionPolicy(
            name="takeover",
            trigger=SupervisionTrigger(confidence_below=2.0),
            action=SupervisionAction.TAKEOVER,
        ))
        loop = _make_loop(hitl_policy=engine)
        ok, msg = loop._evaluate_hitl("tool", {})
        assert ok is False
        assert "takeover" in json.loads(msg)["hitl_action"]

    def test_blocked_message_is_valid_json(self):
        engine = HITLPolicyEngine(db=None)
        engine.add_policy(SupervisionPolicy(
            name="gate",
            trigger=SupervisionTrigger(confidence_below=2.0),
            action=SupervisionAction.REVIEW_AND_EDIT,
        ))
        loop = _make_loop(hitl_policy=engine)
        ok, msg = loop._evaluate_hitl("tool", {})
        assert ok is False
        data = json.loads(msg)
        assert "error" in data
        assert "hitl_action" in data
        assert "triggered_policies" in data


class TestNovelSkillAutoDetection:
    """Core capability: first-time skills are auto-detected as novel."""

    def test_unseen_skill_is_novel(self):
        """A skill never recorded in success_streak is detected as novel."""
        engine = HITLPolicyEngine(db=None)
        engine.add_policy(SupervisionPolicy(
            name="novel-gate",
            trigger=SupervisionTrigger(novel_skill_use=True),
            action=SupervisionAction.APPROVE_REJECT,
        ))
        loop = _make_loop(hitl_policy=engine)
        # "brand_new_tool" has never been seen → is_novel_skill=True → triggers
        ok, msg = loop._evaluate_hitl("brand_new_tool", {})
        assert ok is False
        data = json.loads(msg)
        assert "novel-gate" in data["triggered_policies"]

    def test_seen_skill_is_not_novel(self):
        """A skill with success history is NOT detected as novel."""
        engine = HITLPolicyEngine(db=None)
        engine.add_policy(SupervisionPolicy(
            name="novel-gate",
            trigger=SupervisionTrigger(novel_skill_use=True),
            action=SupervisionAction.APPROVE_REJECT,
        ))
        engine.record_outcome("known_tool", success=True)
        loop = _make_loop(hitl_policy=engine)
        ok, msg = loop._evaluate_hitl("known_tool", {})
        assert ok is True  # not novel → no trigger

    def test_novel_detection_with_decay(self):
        """Novel skill blocked first, then decays after successes."""
        engine = HITLPolicyEngine(db=None, decay_threshold=2)
        engine.add_policy(SupervisionPolicy(
            name="novel-gate",
            trigger=SupervisionTrigger(novel_skill_use=True),
            action=SupervisionAction.APPROVE_REJECT,
        ))
        loop = _make_loop(hitl_policy=engine)

        # First call: novel → blocked
        ok, _ = loop._evaluate_hitl("new_tool", {})
        assert ok is False

        # Record 2 successes
        engine.record_outcome("new_tool", success=True)
        engine.record_outcome("new_tool", success=True)

        # Now: not novel (in streak) + decay → OBSERVE_ONLY → allowed
        ok, _ = loop._evaluate_hitl("new_tool", {})
        assert ok is True

    def test_ctx_overrides_novel(self):
        """Caller can override is_novel_skill."""
        engine = HITLPolicyEngine(db=None)
        engine.add_policy(SupervisionPolicy(
            name="novel-gate",
            trigger=SupervisionTrigger(novel_skill_use=True),
            action=SupervisionAction.APPROVE_REJECT,
        ))
        loop = _make_loop(hitl_policy=engine)
        # Override: force not novel even though unseen
        ok, _ = loop._evaluate_hitl("unseen_tool", {}, is_novel_skill=False)
        assert ok is True


class TestChatLoopConstructor:
    def test_hitl_policy_stored(self):
        engine = HITLPolicyEngine(db=None)
        loop = _make_loop(hitl_policy=engine)
        assert loop.hitl_policy is engine

    def test_hitl_policy_default_none(self):
        loop = _make_loop()
        assert loop.hitl_policy is None


class TestAllPathsHaveHitlCheck:
    """Structural test: _evaluate_hitl is called in all 3 execution paths."""

    def test_evaluate_hitl_called_in_all_paths(self):
        from pathlib import Path

        src = Path("core/agent/chat_loop.py").read_text()
        count = src.count("_evaluate_hitl(")
        # 1 definition + 3 call sites (run_step, stream, planning)
        assert count >= 4, (
            f"Expected >=4 occurrences of _evaluate_hitl( "
            f"(1 def + 3 calls), found {count}"
        )
