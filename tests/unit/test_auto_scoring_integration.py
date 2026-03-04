"""Tests for auto-scoring integration: EventLogger.update_quality_score + ChatLoop wiring."""

from dataclasses import dataclass
from unittest.mock import MagicMock

from core.evaluation.auto_scorer import compute_auto_score


@dataclass
class FakeFirewallResult:
    safe_to_deliver: bool = True
    confidence_score: float = 0.9
    claims_verified: int = 5
    claims_failed: int = 0
    contradictions: list = None
    warnings: list = None
    evidence_count: int = 5

    def __post_init__(self):
        self.contradictions = self.contradictions or []
        self.warnings = self.warnings or []


class TestChatLoopAutoScoring:
    """Test that ChatLoop._log_response triggers auto-scoring when firewall_result is provided."""

    def _make_loop(self):
        mock_llm = MagicMock()
        mock_llm.config = {"model": "test-model"}
        mock_event_logger = MagicMock()
        mock_event = MagicMock()
        mock_event.event_id = "evt_001"
        mock_event_logger.create_llm_response.return_value = mock_event

        from core.agent.chat_loop import ChatLoop
        loop = ChatLoop(
            selector=MagicMock(),
            executor=MagicMock(),
            llm_client=mock_llm,
            event_logger=mock_event_logger,
            context_manager=MagicMock(),
            firewall=MagicMock(),
        )
        return loop, mock_event_logger

    def test_log_response_with_firewall_result_calls_auto_score(self):
        """_log_response with firewall_result → update_quality_score called."""
        loop, mock_logger = self._make_loop()
        fw = FakeFirewallResult(safe_to_deliver=True, confidence_score=0.9)

        loop._log_response(
            user_id="u1", session_id="s1",
            content="Hello world " * 20,
            parent_event_id="p1", causal_chain_id="c1",
            firewall_result=fw,
        )

        mock_logger.update_quality_score.assert_called_once()
        args = mock_logger.update_quality_score.call_args[0]
        assert args[0] == "evt_001"
        assert isinstance(args[1], float)
        assert isinstance(args[2], bool)

    def test_log_response_without_firewall_result_skips_auto_score(self):
        """_log_response without firewall_result → update_quality_score NOT called."""
        loop, mock_logger = self._make_loop()

        loop._log_response(
            user_id="u1", session_id="s1",
            content="Hello", parent_event_id="p1", causal_chain_id="c1",
        )

        mock_logger.update_quality_score.assert_not_called()

    def test_auto_score_failure_is_non_fatal(self):
        """If auto-scoring raises, _log_response still completes."""
        loop, mock_logger = self._make_loop()
        mock_logger.update_quality_score.side_effect = RuntimeError("DB down")
        fw = FakeFirewallResult(safe_to_deliver=True, confidence_score=0.9)

        # Should not raise
        loop._log_response(
            user_id="u1", session_id="s1",
            content="Hello world " * 20,
            parent_event_id="p1", causal_chain_id="c1",
            firewall_result=fw,
        )

        # create_llm_response was still called
        mock_logger.create_llm_response.assert_called_once()

    def test_high_confidence_produces_training_eligible(self):
        """High firewall confidence + reasonable length → training_eligible=True."""
        loop, mock_logger = self._make_loop()
        fw = FakeFirewallResult(safe_to_deliver=True, confidence_score=0.95)

        loop._log_response(
            user_id="u1", session_id="s1",
            content="word " * 100,  # 100 words
            parent_event_id="p1", causal_chain_id="c1",
            firewall_result=fw,
        )

        args = mock_logger.update_quality_score.call_args[0]
        quality_score = args[1]
        training_eligible = args[2]
        assert quality_score >= 4.0
        assert training_eligible is True

    def test_low_confidence_produces_not_training_eligible(self):
        """Low firewall confidence → training_eligible=False."""
        loop, mock_logger = self._make_loop()
        fw = FakeFirewallResult(safe_to_deliver=False, confidence_score=0.2)

        loop._log_response(
            user_id="u1", session_id="s1",
            content="word " * 100,
            parent_event_id="p1", causal_chain_id="c1",
            firewall_result=fw,
        )

        args = mock_logger.update_quality_score.call_args[0]
        assert args[1] < 4.0
        assert args[2] is False


class TestAllPathsPassFirewallResult:
    """Verify every _log_response call site passes firewall_result."""

    def test_all_log_response_calls_have_firewall_result(self):
        """Grep source to ensure no _log_response call omits firewall_result."""
        import re
        from pathlib import Path

        src = Path("core/agent/chat_loop.py").read_text()
        # Find all _log_response( calls
        calls = list(re.finditer(r"self\._log_response\(", src))
        # After Task 0.4 refactor: run_step is now a thin wrapper around run_step_stream,
        # so there are fewer direct _log_response calls (5 instead of 7).
        assert len(calls) >= 5, f"Expected >=5 _log_response calls, found {len(calls)}"

        for match in calls:
            # Extract the full call (up to the closing paren at same indent)
            start = match.start()
            # Find the next occurrence of firewall_result or the closing )
            block_end = src.find("\n        )", start)
            if block_end == -1:
                block_end = src.find("\n            )", start)
            if block_end == -1:
                block_end = src.find("\n                )", start)
            block = src[start:block_end + 20] if block_end != -1 else src[start:start + 500]
            assert "firewall_result=" in block, (
                f"_log_response call at offset {start} missing firewall_result=\n"
                f"Context: {block[:200]}"
            )
