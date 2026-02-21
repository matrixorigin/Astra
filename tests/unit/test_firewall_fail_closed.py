"""Tests for HallucinationFirewall fail-closed behavior in block mode.

Covers: empty response, no claims, extraction failure, snapshot load failure.
All must return safe_to_deliver=False when mode='block'.
"""

from unittest.mock import Mock, patch

import pytest

from core.verification.firewall import HallucinationFirewall, FirewallResult


@pytest.fixture
def firewall():
    db = Mock()
    ctx = Mock()
    with patch("core.verification.firewall.ClaimExtractor"), \
         patch("core.verification.schema.init_hallucination_tables"):
        fw = HallucinationFirewall(db, ctx, llm_client=None, use_llm_extraction=False)
    return fw


class TestBlockModeFailClosed:
    """Block mode must fail-closed on any verification inability."""

    def test_empty_response_blocks(self, firewall):
        r = firewall.verify_response("", "snap_1", mode="block")
        assert r.safe_to_deliver is False
        assert r.confidence_score == 0.0

    def test_whitespace_response_blocks(self, firewall):
        r = firewall.verify_response("   \n  ", "snap_1", mode="block")
        assert r.safe_to_deliver is False

    def test_no_claims_blocks(self, firewall):
        firewall.regex_extractor = Mock()
        firewall.regex_extractor.extract.return_value = []
        r = firewall.verify_response("Hello world", "snap_1", mode="block")
        assert r.safe_to_deliver is False
        assert r.confidence_score == 0.0

    def test_extraction_failure_blocks(self, firewall):
        firewall.regex_extractor = Mock()
        firewall.regex_extractor.extract.side_effect = RuntimeError("boom")
        r = firewall.verify_response("Some text", "snap_1", mode="block")
        assert r.safe_to_deliver is False
        assert r.confidence_score == 0.0

    def test_snapshot_load_failure_blocks(self, firewall):
        firewall.regex_extractor = Mock()
        firewall.regex_extractor.extract.return_value = [Mock(value="claim")]
        firewall.context_manager.load_snapshot.side_effect = RuntimeError("not found")
        r = firewall.verify_response("Some text", "snap_1", mode="block")
        assert r.safe_to_deliver is False

    def test_missing_context_capture_id_blocks(self, firewall):
        r = firewall.verify_response("Some text", "", mode="block")
        assert r.safe_to_deliver is False


class TestWarnModeFailOpen:
    """Warn mode must fail-open (deliver with warnings)."""

    def test_empty_response_passes(self, firewall):
        r = firewall.verify_response("", "snap_1", mode="warn")
        assert r.safe_to_deliver is True
        assert r.confidence_score == 0.0

    def test_no_claims_passes(self, firewall):
        firewall.regex_extractor = Mock()
        firewall.regex_extractor.extract.return_value = []
        r = firewall.verify_response("Hello world", "snap_1", mode="warn")
        assert r.safe_to_deliver is True
        assert r.confidence_score == 0.0

    def test_extraction_failure_passes(self, firewall):
        firewall.regex_extractor = Mock()
        firewall.regex_extractor.extract.side_effect = RuntimeError("boom")
        r = firewall.verify_response("Some text", "snap_1", mode="warn")
        assert r.safe_to_deliver is True

    def test_snapshot_load_failure_passes(self, firewall):
        firewall.regex_extractor = Mock()
        firewall.regex_extractor.extract.return_value = [Mock(value="claim")]
        firewall.context_manager.load_snapshot.side_effect = RuntimeError("not found")
        r = firewall.verify_response("Some text", "snap_1", mode="warn")
        assert r.safe_to_deliver is True


class TestChatLoopFirewallMode:
    """ChatLoop.firewall_mode propagates to verify_response calls."""

    def test_default_mode_is_warn(self):
        from core.agent.chat_loop import ChatLoop
        loop = ChatLoop(
            selector=Mock(), executor=Mock(), llm_client=Mock(),
            event_logger=Mock(), context_manager=Mock(), firewall=Mock(),
        )
        assert loop.firewall_mode == "warn"

    def test_block_mode_accepted(self):
        from core.agent.chat_loop import ChatLoop
        loop = ChatLoop(
            selector=Mock(), executor=Mock(), llm_client=Mock(),
            event_logger=Mock(), context_manager=Mock(), firewall=Mock(),
            firewall_mode="block",
        )
        assert loop.firewall_mode == "block"

    def test_invalid_mode_falls_back_to_warn(self):
        from core.agent.chat_loop import ChatLoop
        loop = ChatLoop(
            selector=Mock(), executor=Mock(), llm_client=Mock(),
            event_logger=Mock(), context_manager=Mock(), firewall=Mock(),
            firewall_mode="invalid",
        )
        assert loop.firewall_mode == "warn"
