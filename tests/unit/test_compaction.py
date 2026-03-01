"""Tests for context compaction."""

import pytest

from core.context.compaction import (
    _TOOL_CLEARED,
    _clear_old_tool_results,
    _summarize_old_turns,
    _truncate_summary,
    compact,
    estimate_tokens,
    needs_compaction,
)


def _msg(role: str, content: str, **kw) -> dict:
    m = {"role": role, "content": content}
    m.update(kw)
    return m


def _long_content(n_chars: int = 4000) -> str:
    return "x" * n_chars


class TestEstimateTokens:
    def test_empty(self):
        assert estimate_tokens([]) == 0

    def test_simple_messages(self):
        msgs = [_msg("user", "hello world")]  # 11 chars → 2 tokens
        assert estimate_tokens(msgs) == 11 // 4

    def test_tool_calls_add_overhead(self):
        msgs = [_msg("assistant", "", tool_calls=[{"id": "1"}, {"id": "2"}])]
        assert estimate_tokens(msgs) == 100  # 50 per tool_call

    def test_none_content(self):
        msgs = [{"role": "assistant", "content": None}]
        assert estimate_tokens(msgs) == 0


class TestNeedsCompaction:
    def test_under_threshold(self):
        msgs = [_msg("user", "hi")]
        assert not needs_compaction(msgs, 1000)

    def test_over_threshold(self):
        # 50% of 100 = 50 tokens → need >200 chars
        msgs = [_msg("user", _long_content(400))]
        assert needs_compaction(msgs, 100)

    def test_exactly_at_threshold(self):
        # 50 tokens = 200 chars, estimate_tokens(200 chars) = 50
        msgs = [_msg("user", "x" * 200)]
        assert not needs_compaction(msgs, 100)  # 50 == 50, not >


class TestClearOldToolResults:
    def test_clears_old_long_tool_results(self):
        msgs = [
            _msg("system", "sys"),
            _msg("tool", _long_content(1000), tool_call_id="t1"),
            _msg("assistant", "processed"),
            _msg("user", "next"),
            _msg("tool", _long_content(1000), tool_call_id="t2"),
            _msg("assistant", "done"),
        ]
        result = _clear_old_tool_results(msgs, preserve_recent=2)
        # Old tool results (index 1) cleared, recent (index 4) preserved
        assert result[1]["content"] == _TOOL_CLEARED
        assert len(result[4]["content"]) == 1000

    def test_preserves_short_tool_results(self):
        msgs = [
            _msg("system", "sys"),
            _msg("tool", "ok", tool_call_id="t1"),
            _msg("user", "next"),
        ]
        result = _clear_old_tool_results(msgs, preserve_recent=1)
        assert result[1]["content"] == "ok"  # <200 chars, not cleared

    def test_no_mutation_of_original(self):
        msgs = [
            _msg("tool", _long_content(500), tool_call_id="t1"),
            _msg("user", "hi"),
        ]
        original_content = msgs[0]["content"]
        # compact() copies, but _clear_old_tool_results works in-place on its input
        compact(msgs, 10)  # triggers compaction
        # Original should be unchanged since compact() copies
        assert msgs[0]["content"] == original_content


class TestSummarizeOldTurns:
    def test_summarizes_old_keeps_recent(self):
        msgs = [
            _msg("system", "You are helpful"),
            _msg("user", "question 1"),
            _msg("assistant", "answer 1"),
            _msg("user", "question 2"),
            _msg("assistant", "answer 2"),
            _msg("user", "question 3"),
            _msg("assistant", "answer 3"),
        ]
        result = _summarize_old_turns(msgs, preserve_recent=2)
        # system + summary + 2 recent
        assert len(result) == 4
        assert result[0]["role"] == "system"
        assert result[0]["content"] == "You are helpful"
        assert "[Compacted conversation summary]" in result[1]["content"]
        assert result[2] == msgs[5]  # question 3
        assert result[3] == msgs[6]  # answer 3

    def test_no_system_message(self):
        msgs = [
            _msg("user", "q1"),
            _msg("assistant", "a1"),
            _msg("user", "q2"),
            _msg("assistant", "a2"),
        ]
        result = _summarize_old_turns(msgs, preserve_recent=2)
        assert result[0]["content"].startswith("[Compacted")
        assert len(result) == 3  # summary + 2 recent

    def test_too_few_messages(self):
        msgs = [_msg("system", "sys"), _msg("user", "hi")]
        result = _summarize_old_turns(msgs, preserve_recent=2)
        assert result == msgs  # unchanged

    def test_llm_summarize_called(self):
        msgs = [
            _msg("system", "sys"),
            _msg("user", "old question"),
            _msg("assistant", "old answer"),
            _msg("user", "new"),
            _msg("assistant", "new answer"),
        ]
        summarizer = lambda text: "LLM summary of conversation"
        result = _summarize_old_turns(msgs, preserve_recent=2, llm_summarize=summarizer)
        assert "LLM summary" in result[1]["content"]

    def test_llm_summarize_failure_falls_back(self):
        msgs = [
            _msg("system", "sys"),
            _msg("user", "old"),
            _msg("assistant", "old"),
            _msg("user", "new"),
            _msg("assistant", "new"),
        ]
        def bad_summarizer(text):
            raise RuntimeError("LLM down")

        result = _summarize_old_turns(msgs, preserve_recent=2, llm_summarize=bad_summarizer)
        assert "[Compacted" in result[1]["content"]
        assert "old" in result[1]["content"]  # truncation fallback includes content


class TestTruncateSummary:
    def test_short_text_unchanged(self):
        parts = ["user: hi", "assistant: hello"]
        assert _truncate_summary(parts) == "user: hi\nassistant: hello"

    def test_long_text_truncated(self):
        parts = ["x" * 3000]
        result = _truncate_summary(parts, max_chars=100)
        assert len(result) < 200
        assert "[...earlier conversation truncated]" in result


class TestCompact:
    def test_no_compaction_needed(self):
        msgs = [_msg("user", "hi")]
        result = compact(msgs, 10000)
        assert result == msgs

    def test_phase1_sufficient(self):
        """Tool clearing alone brings it under budget."""
        msgs = [
            _msg("system", "sys"),
            _msg("tool", _long_content(2000), tool_call_id="t1"),
            _msg("assistant", "processed that"),
            _msg("user", "next question"),
            _msg("assistant", "final"),
        ]
        result = compact(msgs, 400, preserve_recent=2)
        # Old tool result (index 1) should be cleared
        tool_msg = next(m for m in result if m["role"] == "tool")
        assert tool_msg["content"] == _TOOL_CLEARED

    def test_phase2_summarization(self):
        """Both phases needed."""
        msgs = [_msg("system", "sys")]
        for i in range(20):
            msgs.append(_msg("user", f"question {i} " + "x" * 200))
            msgs.append(_msg("assistant", f"answer {i} " + "y" * 200))

        result = compact(msgs, 500, preserve_recent=4)
        # Should have: system + summary + 4 recent
        assert len(result) <= 6
        assert result[0]["content"] == "sys"
        assert "[Compacted" in result[1]["content"]

    def test_original_not_mutated(self):
        msgs = [
            _msg("system", "sys"),
            _msg("tool", _long_content(2000), tool_call_id="t1"),
            _msg("user", "hi"),
        ]
        original_tool_content = msgs[1]["content"]
        compact(msgs, 200)
        assert msgs[1]["content"] == original_tool_content
