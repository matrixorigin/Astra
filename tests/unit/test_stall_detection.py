"""Tests for ChatLoop stall detection.

Covers:
- _record_round_tools signature extraction
- _detect_stall with exact-match window logic
- Edge cases: empty calls, single tool, mixed tools
- No false positives on genuinely different queries
"""

import pytest

from core.agent.chat_loop import ChatLoop


def _make_tc(name: str, args: str = "") -> dict:
    """Build a minimal tool_call dict."""
    return {"function": {"name": name, "arguments": args}}


@pytest.fixture
def loop():
    """Bare ChatLoop with only stall-detection state initialised."""
    obj = ChatLoop.__new__(ChatLoop)
    obj._round_tool_sigs = []
    return obj


class TestRecordRoundTools:

    def test_extracts_name_and_args(self, loop):
        loop._record_round_tools([_make_tc("grep", '{"pattern":"foo"}')])
        assert len(loop._round_tool_sigs) == 1
        sig = next(iter(loop._round_tool_sigs[0]))
        assert sig == 'grep:{"pattern":"foo"}'

    def test_multiple_tools_in_one_round(self, loop):
        loop._record_round_tools([
            _make_tc("grep", '{"pattern":"a"}'),
            _make_tc("fs_read", '{"path":"/x"}'),
        ])
        assert len(loop._round_tool_sigs[0]) == 2

    def test_empty_args_default(self, loop):
        loop._record_round_tools([_make_tc("grep")])
        sig = next(iter(loop._round_tool_sigs[0]))
        assert sig == "grep:"


class TestDetectStall:

    def test_no_stall_below_window(self, loop):
        """Fewer rounds than window → never stall."""
        loop._record_round_tools([_make_tc("grep", '{"p":"a"}')])
        loop._record_round_tools([_make_tc("grep", '{"p":"a"}')])
        # Window is 3, only 2 rounds recorded
        assert loop._detect_stall() is False

    def test_no_stall_different_args(self, loop):
        """Same tool, different arguments → not a stall."""
        loop._record_round_tools([_make_tc("grep", '{"pattern":"foo"}')])
        loop._record_round_tools([_make_tc("grep", '{"pattern":"bar"}')])
        loop._record_round_tools([_make_tc("grep", '{"pattern":"baz"}')])
        assert loop._detect_stall() is False

    def test_no_stall_different_tools(self, loop):
        """Different tools each round → not a stall."""
        loop._record_round_tools([_make_tc("grep", '{"p":"x"}')])
        loop._record_round_tools([_make_tc("fs_read", '{"path":"y"}')])
        loop._record_round_tools([_make_tc("shell", '{"cmd":"z"}')])
        assert loop._detect_stall() is False

    def test_stall_identical_rounds(self, loop):
        """Exact same call signature 3 rounds in a row → stall."""
        tc = [_make_tc("grep", '{"pattern":"lost_func"}')]
        loop._record_round_tools(tc)
        loop._record_round_tools(tc)
        loop._record_round_tools(tc)
        assert loop._detect_stall() is True

    def test_stall_multi_tool_identical(self, loop):
        """Multiple tools, all identical across window → stall."""
        tc = [_make_tc("grep", '{"p":"a"}'), _make_tc("fs_read", '{"path":"/b"}')]
        loop._record_round_tools(tc)
        loop._record_round_tools(tc)
        loop._record_round_tools(tc)
        assert loop._detect_stall() is True

    def test_no_stall_if_one_round_differs(self, loop):
        """Two identical + one different → no stall (window requires all 3)."""
        tc = [_make_tc("grep", '{"p":"a"}')]
        loop._record_round_tools(tc)
        loop._record_round_tools(tc)
        loop._record_round_tools([_make_tc("grep", '{"p":"b"}')])
        assert loop._detect_stall() is False

    def test_stall_only_checks_last_window(self, loop):
        """Earlier different rounds don't prevent stall in the last window."""
        loop._record_round_tools([_make_tc("fs_read", '{"path":"x"}')])
        tc = [_make_tc("grep", '{"p":"stuck"}')]
        loop._record_round_tools(tc)
        loop._record_round_tools(tc)
        loop._record_round_tools(tc)
        assert loop._detect_stall() is True

    def test_empty_tool_calls(self, loop):
        """Empty tool_calls produce empty sig set — 3 empty rounds = stall."""
        loop._record_round_tools([])
        loop._record_round_tools([])
        loop._record_round_tools([])
        assert loop._detect_stall() is True

    def test_reset_clears_state(self, loop):
        """After reset, stall detection starts fresh."""
        tc = [_make_tc("grep", '{"p":"a"}')]
        loop._record_round_tools(tc)
        loop._record_round_tools(tc)
        loop._round_tool_sigs = []  # simulates _reset_breaker_state
        loop._record_round_tools(tc)
        assert loop._detect_stall() is False
