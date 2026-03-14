"""Tests: reasoning_content stored as dedicated DB column, not in metadata.

Verifies:
- append_recovered_events reads reasoning_content from row[3] (column), not metadata
- reasoning_content in metadata is ignored (migration: old data won't bleed through)
- reasoning_content correctly attached to assistant message
- rows without reasoning_content (None) produce no reasoning_content key
- trailing tool_calls flush preserves reasoning_content from column
"""

import json
import pytest

from core.history_utils import append_recovered_events


def _row(event_type, content, metadata=None, reasoning_content=None):
    """Build a 4-tuple matching the new DB query: (event_type, content, metadata, reasoning_content)."""
    return (event_type, content, json.dumps(metadata or {}), reasoning_content)


class TestReasoningContentColumn:
    def test_reasoning_content_from_column_attached_to_assistant(self):
        """reasoning_content from row[3] must appear on the assistant message."""
        rows = [
            _row("user_query", "hello"),
            _row(
                "tool_call",
                json.dumps({"tool_call_id": "tc1", "name": "read_file", "arguments": "{}"}),
                metadata={"tool_call_id": "tc1", "name": "read_file", "source": "edge"},
                reasoning_content="I should read the file first",
            ),
            _row(
                "tool_result",
                json.dumps({"result": "file contents"}),
                metadata={"tool_call_id": "tc1", "name": "read_file"},
            ),
        ]
        history = append_recovered_events([], rows)

        # user message
        assert history[0] == {"role": "user", "content": "hello"}
        # assistant with tool_calls
        asst = history[1]
        assert asst["role"] == "assistant"
        assert asst["reasoning_content"] == "I should read the file first"
        assert len(asst["tool_calls"]) == 1
        assert asst["tool_calls"][0]["id"] == "tc1"
        # tool result
        assert history[2]["role"] == "tool"
        assert history[2]["tool_call_id"] == "tc1"

    def test_reasoning_content_in_metadata_is_ignored(self):
        """Old metadata-based reasoning_content must NOT be read (migration: column is source of truth)."""
        rows = [
            _row("user_query", "hello"),
            _row(
                "tool_call",
                json.dumps({"tool_call_id": "tc1", "name": "fn", "arguments": "{}"}),
                # Old format: reasoning_content in metadata
                metadata={
                    "tool_call_id": "tc1",
                    "name": "fn",
                    "source": "edge",
                    "reasoning_content": "old metadata reasoning",
                },
                reasoning_content=None,
            ),  # column is None
            _row(
                "tool_result",
                json.dumps({"result": "ok"}),
                metadata={"tool_call_id": "tc1", "name": "fn"},
            ),
        ]
        history = append_recovered_events([], rows)

        asst = history[1]
        assert asst["role"] == "assistant"
        # Must NOT have reasoning_content (column is None, metadata is ignored)
        assert "reasoning_content" not in asst

    def test_no_reasoning_content_produces_no_key(self):
        """Normal models (no reasoning) must not have reasoning_content key on assistant."""
        rows = [
            _row("user_query", "hi"),
            _row(
                "tool_call",
                json.dumps({"tool_call_id": "tc1", "name": "fn", "arguments": "{}"}),
                metadata={"tool_call_id": "tc1", "name": "fn", "source": "edge"},
                reasoning_content=None,
            ),
            _row(
                "tool_result",
                json.dumps({"result": "done"}),
                metadata={"tool_call_id": "tc1", "name": "fn"},
            ),
        ]
        history = append_recovered_events([], rows)
        asst = history[1]
        assert "reasoning_content" not in asst

    def test_reasoning_content_only_on_first_tool_call_in_batch(self):
        """Only the first tool_call row carries reasoning_content; batch must have it once."""
        rows = [
            _row("user_query", "do two things"),
            _row(
                "tool_call",
                json.dumps({"tool_call_id": "tc1", "name": "fn1", "arguments": "{}"}),
                metadata={"tool_call_id": "tc1", "name": "fn1", "source": "edge"},
                reasoning_content="thinking...",
            ),
            _row(
                "tool_call",
                json.dumps({"tool_call_id": "tc2", "name": "fn2", "arguments": "{}"}),
                metadata={"tool_call_id": "tc2", "name": "fn2", "source": "edge"},
                reasoning_content=None,
            ),  # second tool_call has no reasoning
            _row(
                "tool_result",
                json.dumps({"result": "r1"}),
                metadata={"tool_call_id": "tc1", "name": "fn1"},
            ),
            _row(
                "tool_result",
                json.dumps({"result": "r2"}),
                metadata={"tool_call_id": "tc2", "name": "fn2"},
            ),
        ]
        history = append_recovered_events([], rows)

        asst = history[1]
        assert asst["role"] == "assistant"
        assert asst["reasoning_content"] == "thinking..."
        assert len(asst["tool_calls"]) == 2

    def test_trailing_tool_calls_flush_preserves_reasoning(self):
        """Trailing tool_calls (no tool_result) must still carry reasoning_content."""
        rows = [
            _row("user_query", "do something"),
            _row(
                "tool_call",
                json.dumps({"tool_call_id": "tc1", "name": "fn", "arguments": "{}"}),
                metadata={"tool_call_id": "tc1", "name": "fn", "source": "edge"},
                reasoning_content="deep thought",
            ),
            # No tool_result — simulates API crash mid-execution
        ]
        history = append_recovered_events([], rows)

        assert len(history) == 2
        asst = history[1]
        assert asst["role"] == "assistant"
        assert asst["reasoning_content"] == "deep thought"
        assert len(asst["tool_calls"]) == 1

    def test_three_tuple_rows_still_work(self):
        """Backward compat: 3-tuple rows (no reasoning_content column) must not crash."""
        rows = [
            ("user_query", "hello", json.dumps({})),
            (
                "tool_call",
                json.dumps({"tool_call_id": "tc1", "name": "fn", "arguments": "{}"}),
                json.dumps({"tool_call_id": "tc1", "name": "fn", "source": "edge"}),
            ),
            (
                "tool_result",
                json.dumps({"result": "ok"}),
                json.dumps({"tool_call_id": "tc1", "name": "fn"}),
            ),
        ]
        history = append_recovered_events([], rows)
        asst = history[1]
        assert asst["role"] == "assistant"
        assert "reasoning_content" not in asst

    def test_full_conversation_with_reasoning_and_text_response(self):
        """Multi-turn: reasoning on tool turn, then plain llm_response."""
        rows = [
            _row("user_query", "turn 1"),
            _row(
                "tool_call",
                json.dumps({"tool_call_id": "tc1", "name": "fn", "arguments": "{}"}),
                metadata={"tool_call_id": "tc1", "name": "fn", "source": "edge"},
                reasoning_content="let me think",
            ),
            _row(
                "tool_result",
                json.dumps({"result": "data"}),
                metadata={"tool_call_id": "tc1", "name": "fn"},
            ),
            _row("llm_response", "Here is the answer"),
            _row("user_query", "turn 2"),
            _row("llm_response", "Simple reply"),
        ]
        history = append_recovered_events([], rows)

        assert history[0] == {"role": "user", "content": "turn 1"}
        assert history[1]["reasoning_content"] == "let me think"
        assert history[2]["role"] == "tool"
        assert history[3] == {"role": "assistant", "content": "Here is the answer"}
        assert "reasoning_content" not in history[3]
        assert history[4] == {"role": "user", "content": "turn 2"}
        assert history[5] == {"role": "assistant", "content": "Simple reply"}
        assert "reasoning_content" not in history[5]

    def test_text_only_thinking_response_has_reasoning(self):
        """Text-only response with reasoning_content on llm_response event."""
        rows = [
            _row("user_query", "explain something"),
            _row(
                "llm_response",
                "Here is my explanation",
                reasoning_content="let me reason about this",
            ),
        ]
        history = append_recovered_events([], rows)

        assert history[0] == {"role": "user", "content": "explain something"}
        asst = history[1]
        assert asst["role"] == "assistant"
        assert asst["content"] == "Here is my explanation"
        assert asst["reasoning_content"] == "let me reason about this"

    def test_llm_response_no_reasoning_has_no_key(self):
        """llm_response without reasoning_content must not have the key."""
        rows = [
            _row("user_query", "hi"),
            _row("llm_response", "hello", reasoning_content=None),
        ]
        history = append_recovered_events([], rows)
        assert "reasoning_content" not in history[1]
