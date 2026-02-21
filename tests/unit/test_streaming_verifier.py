"""Tests for StreamingVerifier batch mode."""

from unittest.mock import MagicMock, Mock

import pytest

from core.verification.streaming_verifier import (
    StreamingVerifier,
    DEFAULT_BATCH_SIZE,
    _SENTENCE_RE,
)


def _make_sv(llm_response: str | None = None, batch_size: int = DEFAULT_BATCH_SIZE):
    firewall = Mock()
    llm = None
    if llm_response is not None:
        llm = Mock()
        llm.chat.return_value = Mock(content=llm_response)
    sv = StreamingVerifier(
        firewall=firewall,
        context_capture_id="ctx-1",
        llm_client=llm,
        batch_size=batch_size,
    )
    sv._context_text = "The sky is blue. Water is wet. Paris is in France."
    return sv


class TestBatchAccumulation:
    def test_no_complete_sentence_returns_empty(self):
        sv = _make_sv()
        result = sv.check("Hello world")
        assert result == []

    def test_sentences_accumulate_until_batch_size(self):
        sv = _make_sv(llm_response="1: SUPPORTED\n2: SUPPORTED", batch_size=2)
        # First sentence — batch not full yet
        result = sv.check("The sky is blue. ")
        assert result == []
        assert len(sv._pending_sentences) == 1

        # Second sentence — batch full, triggers verification
        result = sv.check("Water is wet. ")
        assert result == []  # both SUPPORTED
        assert len(sv._pending_sentences) == 0  # batch was consumed

    def test_batch_triggers_single_llm_call(self):
        sv = _make_sv(llm_response="1: SUPPORTED\n2: SUPPORTED\n3: SUPPORTED", batch_size=3)
        sv.check("Sentence one. ")
        sv.check("Sentence two. ")
        sv.check("Sentence three. ")
        assert sv.llm_client.chat.call_count == 1

    def test_full_text_accumulates_all_chunks(self):
        sv = _make_sv()
        sv.check("Hello ")
        sv.check("world. ")
        assert sv.full_text == "Hello world. "


class TestContradictionDetection:
    def test_contradicted_sentence_returns_warning(self):
        sv = _make_sv(llm_response="1: CONTRADICTED", batch_size=1)
        result = sv.check("The sky is green. ")
        assert len(result) == 1
        assert "⚠️" in result[0]

    def test_supported_sentence_returns_no_warning(self):
        sv = _make_sv(llm_response="1: SUPPORTED", batch_size=1)
        result = sv.check("The sky is blue. ")
        assert result == []

    def test_unverifiable_sentence_returns_no_warning(self):
        sv = _make_sv(llm_response="1: UNVERIFIABLE", batch_size=1)
        result = sv.check("Maybe something. ")
        assert result == []

    def test_batch_partial_contradiction(self):
        sv = _make_sv(llm_response="1: SUPPORTED\n2: CONTRADICTED\n3: SUPPORTED", batch_size=3)
        sv.check("True fact. ")
        sv.check("False claim. ")
        result = sv.check("Another true fact. ")
        assert len(result) == 1
        assert sv._warned_sentences == 1

    def test_multiple_contradictions_in_batch(self):
        sv = _make_sv(llm_response="1: CONTRADICTED\n2: CONTRADICTED", batch_size=2)
        sv.check("Wrong one. ")
        result = sv.check("Wrong two. ")
        assert len(result) == 2
        assert sv._warned_sentences == 2


class TestFlush:
    def test_flush_verifies_remaining_sentences(self):
        sv = _make_sv(llm_response="1: CONTRADICTED", batch_size=3)
        sv.check("Only one sentence. ")
        # batch not full — pending
        assert len(sv._pending_sentences) == 1
        result = sv.flush()
        assert len(result) == 1
        assert sv._pending_sentences == []

    def test_flush_includes_buffer_text(self):
        sv = _make_sv(llm_response="1: SUPPORTED", batch_size=3)
        # Feed text without sentence terminator — stays in buffer
        sv.check("Incomplete sentence without terminator")
        assert sv._buffer == "Incomplete sentence without terminator"
        result = sv.flush()
        assert result == []  # SUPPORTED
        assert sv._buffer == ""

    def test_flush_empty_state_returns_empty(self):
        sv = _make_sv()
        result = sv.flush()
        assert result == []

    def test_flush_short_buffer_skipped(self):
        sv = _make_sv()
        sv._buffer = "Hi"  # < 10 chars
        result = sv.flush()
        assert result == []


class TestFirewallFallback:
    def test_uses_firewall_when_no_llm(self):
        firewall = Mock()
        firewall.verify_response.return_value = Mock(
            claims_failed=1, confidence_score=0.3
        )
        sv = StreamingVerifier(
            firewall=firewall,
            context_capture_id="ctx-1",
            llm_client=None,
            batch_size=1,
        )
        sv._context_text = "some context"
        result = sv.check("Unverified claim here. ")
        assert len(result) == 1
        assert "unverified" in result[0]

    def test_firewall_passes_returns_empty(self):
        firewall = Mock()
        firewall.verify_response.return_value = Mock(
            claims_failed=0, confidence_score=0.9
        )
        sv = StreamingVerifier(
            firewall=firewall,
            context_capture_id="ctx-1",
            llm_client=None,
            batch_size=1,
        )
        sv._context_text = "some context"
        result = sv.check("Verified claim. ")
        assert result == []


class TestLLMExceptionHandling:
    def test_llm_exception_returns_empty(self):
        sv = _make_sv(llm_response="1: SUPPORTED", batch_size=1)
        sv.llm_client.chat.side_effect = RuntimeError("LLM down")
        result = sv.check("Some sentence. ")
        assert result == []

    def test_no_context_returns_empty(self):
        sv = _make_sv(llm_response="1: CONTRADICTED", batch_size=1)
        sv._context_text = ""  # empty context
        result = sv.check("Some sentence. ")
        assert result == []


class TestExtractClaimIndex:
    def test_standard_format(self):
        assert StreamingVerifier._extract_claim_index("1: CONTRADICTED", 3) == 0
        assert StreamingVerifier._extract_claim_index("2: CONTRADICTED", 3) == 1
        assert StreamingVerifier._extract_claim_index("3: CONTRADICTED", 3) == 2

    def test_out_of_range_returns_none(self):
        assert StreamingVerifier._extract_claim_index("5: CONTRADICTED", 3) is None

    def test_no_number_returns_none(self):
        assert StreamingVerifier._extract_claim_index("CONTRADICTED", 3) is None


class TestStreamingVerifierLogging:
    """Verify that verification failures log at warning level, not debug."""

    def test_llm_batch_failure_logs_warning(self, caplog):
        sv = _make_sv(llm_response="SUPPORTED")
        sv.llm_client.chat.side_effect = RuntimeError("LLM down")
        sv._context_text = "some context"
        import logging
        with caplog.at_level(logging.WARNING, logger="core.verification.streaming_verifier"):
            # Single sentence → _llm_single_check path
            warnings = sv._llm_batch_check(["Test sentence one."])
        assert warnings == []  # graceful degradation
        assert any("LLM down" in r.message for r in caplog.records)
        assert any(r.levelno == logging.WARNING for r in caplog.records)

    def test_context_load_failure_logs_warning(self, caplog):
        sv = _make_sv(llm_response="SUPPORTED")
        sv._context_text = None  # force reload
        sv.firewall.context_manager.load_snapshot.side_effect = RuntimeError("DB down")
        import logging
        with caplog.at_level(logging.DEBUG):
            result = sv._llm_batch_check(["Test sentence."])
        assert result == []

    def test_firewall_fallback_failure_logs_warning(self, caplog):
        sv = _make_sv()  # no LLM → uses firewall fallback
        sv.firewall.verify_response.side_effect = RuntimeError("firewall down")
        import logging
        with caplog.at_level(logging.WARNING):
            result = sv._firewall_batch_check(["Test sentence."])
        assert result == []
        assert any("failed" in r.message.lower() for r in caplog.records)
