"""Regression tests for _accumulate_tool_calls truncation detection."""

from types import SimpleNamespace

from core.llm.providers import _accumulate_tool_calls


def _make_chunk(*, delta_content=None, delta_tool_calls=None, finish_reason=None, usage=None):
    """Build a minimal OpenAI-compatible streaming chunk."""
    if usage:
        return SimpleNamespace(usage=usage, choices=[])
    delta = SimpleNamespace(
        content=delta_content,
        tool_calls=delta_tool_calls,
        reasoning_content=None,
    )
    choice = SimpleNamespace(delta=delta, finish_reason=finish_reason)
    return SimpleNamespace(usage=None, choices=[choice])


def _make_tc_delta(index, *, id=None, name=None, arguments=None, type=None):
    func = SimpleNamespace(name=name, arguments=arguments)
    return SimpleNamespace(index=index, id=id, type=type, function=func)


class TestTruncationDetection:
    """finish_reason='length' marks tool_calls with _truncated flag."""

    def test_truncated_tool_call_has_flag(self):
        chunks = [
            _make_chunk(delta_tool_calls=[_make_tc_delta(0, id="tc1", name="write_file", type="function")]),
            _make_chunk(delta_tool_calls=[_make_tc_delta(0, arguments='{"path": "/tmp/x", "content": "hel')]),
            _make_chunk(finish_reason="length"),
        ]
        events = list(_accumulate_tool_calls(iter(chunks)))
        tc_events = [e for e in events if e["type"] == "tool_call"]
        assert len(tc_events) == 1
        assert tc_events[0]["data"]["_truncated"] is True

    def test_normal_tool_call_no_flag(self):
        chunks = [
            _make_chunk(delta_tool_calls=[_make_tc_delta(0, id="tc1", name="read_file", type="function")]),
            _make_chunk(delta_tool_calls=[_make_tc_delta(0, arguments='{"path": "/tmp/x"}')]),
            _make_chunk(finish_reason="stop"),
        ]
        events = list(_accumulate_tool_calls(iter(chunks)))
        tc_events = [e for e in events if e["type"] == "tool_call"]
        assert len(tc_events) == 1
        assert "_truncated" not in tc_events[0]["data"]

    def test_text_only_no_truncation_issue(self):
        """Pure text response truncated by length — no tool_calls, no crash."""
        chunks = [
            _make_chunk(delta_content="Hello "),
            _make_chunk(delta_content="world"),
            _make_chunk(finish_reason="length"),
        ]
        events = list(_accumulate_tool_calls(iter(chunks)))
        assert all(e["type"] == "text" for e in events)
