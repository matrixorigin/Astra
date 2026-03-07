"""Tests for tool-call-safe history splitting.

Reproduces the root cause of the 400 error:
  'Invalid request: tool_call_id is not found'

The error occurs when _build_retrieval_view or _summarize_old_turns naively
slices history at a fixed offset, splitting an assistant(tool_calls) message
from its corresponding tool messages.  The LLM API then sees orphaned tool
messages referencing a tool_call_id that doesn't exist in any assistant message.

These tests verify that find_tool_call_safe_split, _build_retrieval_view, and
_summarize_old_turns never produce such orphaned tool messages.
"""

from __future__ import annotations

import pytest

from core.history_utils import find_tool_call_safe_split


# ── Helpers ──────────────────────────────────────────────────────────

def _sys(content: str = "system") -> dict:
    return {"role": "system", "content": content}


def _user(content: str) -> dict:
    return {"role": "user", "content": content}


def _assistant(content: str) -> dict:
    return {"role": "assistant", "content": content}


def _assistant_tc(tc_ids: list[str]) -> dict:
    """Assistant message with tool_calls."""
    return {
        "role": "assistant",
        "content": "",
        "tool_calls": [
            {"id": tc_id, "type": "function",
             "function": {"name": f"tool_{tc_id}", "arguments": "{}"}}
            for tc_id in tc_ids
        ],
    }


def _tool(tc_id: str, content: str = "result") -> dict:
    return {"role": "tool", "tool_call_id": tc_id, "content": content}


def _validate_no_orphaned_tools(messages: list[dict]) -> None:
    """Assert every tool message's tool_call_id exists in a preceding assistant."""
    available_tc_ids: set[str] = set()
    for m in messages:
        if m.get("role") == "assistant" and m.get("tool_calls"):
            for tc in m["tool_calls"]:
                available_tc_ids.add(tc["id"])
        elif m.get("role") == "tool":
            tc_id = m.get("tool_call_id", "")
            assert tc_id in available_tc_ids, (
                f"Orphaned tool message: tool_call_id={tc_id!r} not in any "
                f"preceding assistant.tool_calls. Available: {available_tc_ids}"
            )


# ── find_tool_call_safe_split unit tests ─────────────────────────────

class TestFindToolCallSafeSplit:
    def test_no_tool_calls_returns_naive_split(self):
        msgs = [_sys(), _user("q1"), _assistant("a1"), _user("q2"), _assistant("a2")]
        idx = find_tool_call_safe_split(msgs, 2)
        assert idx == 3  # naive: len(5) - 2 = 3

    def test_split_lands_on_tool_message_moves_earlier(self):
        """Naive split at index 3 would orphan tool(tc1). Must move to index 2."""
        msgs = [
            _sys(),                      # 0
            _user("q1"),                 # 1
            _assistant_tc(["tc1"]),      # 2
            _tool("tc1"),                # 3  ← naive split lands here
            _assistant("answer"),        # 4
            _user("q2"),                 # 5
        ]
        idx = find_tool_call_safe_split(msgs, 3)
        # Naive: 6 - 3 = 3, but msgs[3] is tool → move back to 2
        assert idx == 2
        _validate_no_orphaned_tools(msgs[idx:])

    def test_split_lands_between_multiple_tools(self):
        """Naive split orphans second tool of a parallel tool_call."""
        msgs = [
            _sys(),                          # 0
            _user("q1"),                     # 1
            _assistant_tc(["tc1", "tc2"]),   # 2
            _tool("tc1"),                    # 3  ← naive split lands here
            _tool("tc2"),                    # 4
            _assistant("done"),              # 5
            _user("q2"),                     # 6
        ]
        idx = find_tool_call_safe_split(msgs, 4)
        # Naive: 7 - 4 = 3, msgs[3] is tool → back to 2
        assert idx == 2
        _validate_no_orphaned_tools(msgs[idx:])

    def test_split_on_assistant_is_safe(self):
        msgs = [
            _sys(),                      # 0
            _user("q1"),                 # 1
            _assistant_tc(["tc1"]),      # 2
            _tool("tc1"),                # 3
            _assistant("answer"),        # 4  ← naive split lands here
            _user("q2"),                 # 5
        ]
        idx = find_tool_call_safe_split(msgs, 2)
        assert idx == 4  # safe, no adjustment needed
        _validate_no_orphaned_tools(msgs[idx:])

    def test_target_larger_than_list(self):
        msgs = [_user("q1"), _assistant("a1")]
        assert find_tool_call_safe_split(msgs, 10) == 0

    def test_target_zero(self):
        msgs = [_user("q1"), _assistant("a1")]
        assert find_tool_call_safe_split(msgs, 0) == 0

    def test_entire_history_is_one_tool_group(self):
        msgs = [
            _assistant_tc(["tc1"]),  # 0
            _tool("tc1"),            # 1
        ]
        idx = find_tool_call_safe_split(msgs, 1)
        # Naive: 2 - 1 = 1, msgs[1] is tool → back to 0
        assert idx == 0


# ── _build_retrieval_view end-to-end ─────────────────────────────────

class TestBuildRetrievalViewToolCallSafety:
    """Reproduce the exact failure: retrieval view orphans tool messages."""

    def _build_history_with_tool_call_at_boundary(self) -> list[dict]:
        """Build history where naive -8 split lands on a tool message.

        Layout (15 messages, naive split at index 7):
          0: system
          1: user("q1")
          2: assistant(tc=["tc1"])
          3: tool("tc1")
          4: assistant("a1")
          5: user("q2")
          6: assistant(tc=["tc2","tc3"])  ← assistant with tool_calls
          7: tool("tc2")                 ← naive -8 split lands HERE
          8: tool("tc3")
          9: assistant("a2")
         10: user("q3")
         11: assistant(tc=["tc4"])
         12: tool("tc4")
         13: assistant("a3")
         14: user("q4")
        """
        return [
            _sys("You are helpful"),
            _user("q1"),
            _assistant_tc(["tc1"]),
            _tool("tc1", "result1"),
            _assistant("a1"),
            _user("q2"),
            _assistant_tc(["tc2", "tc3"]),
            _tool("tc2", "result2"),
            _tool("tc3", "result3"),
            _assistant("a2"),
            _user("q3"),
            _assistant_tc(["tc4"]),
            _tool("tc4", "result4"),
            _assistant("a3"),
            _user("q4"),
        ]

    def test_retrieval_view_no_orphaned_tools(self, db_factory):
        """The exact bug: naive history[-8:] orphans tool(tc2) and tool(tc3)."""
        from api.routers.chat import _build_retrieval_view, _RECENT_MESSAGES_KEEP

        history = self._build_history_with_tool_call_at_boundary()
        assert len(history) >= 14  # triggers retrieval

        current_messages = [_user("follow up question")]

        with db_factory() as db:
            result, _ = _build_retrieval_view(
                history, "test-safe-split", current_messages, db,
            )

        # The critical assertion: no orphaned tool messages
        _validate_no_orphaned_tools(result)

    def test_retrieval_view_still_trims(self, db_factory):
        """Safe split must still trim — not just return full history."""
        from api.routers.chat import _build_retrieval_view

        history = self._build_history_with_tool_call_at_boundary()
        current_messages = [_user("follow up")]

        with db_factory() as db:
            result, _ = _build_retrieval_view(
                history, "test-trim", current_messages, db,
            )

        assert len(result) < len(history), \
            f"Should trim: result={len(result)}, history={len(history)}"


# ── _summarize_old_turns end-to-end ──────────────────────────────────

class TestSummarizeOldTurnsToolCallSafety:
    """Reproduce the failure in compaction's _summarize_old_turns."""

    def test_summarize_no_orphaned_tools(self):
        """Naive cutoff = len - preserve_recent splits a tool_call group."""
        from core.context.compaction import _summarize_old_turns

        # preserve_recent=4 → naive cutoff at index 5
        # But index 5 is tool("tc2"), orphaning it from assistant at index 4
        msgs = [
            _sys("sys"),                     # 0
            _user("q1"),                     # 1
            _assistant("a1"),                # 2
            _user("q2"),                     # 3
            _assistant_tc(["tc2"]),          # 4
            _tool("tc2", "result"),          # 5  ← naive cutoff
            _assistant("a2"),                # 6
            _user("q3"),                     # 7
            _assistant("a3"),                # 8
        ]
        result = _summarize_old_turns(msgs, preserve_recent=4)

        # Extract the "recent" portion (everything after system + summary)
        recent_start = 2  # system + summary
        recent = result[recent_start:]
        _validate_no_orphaned_tools(recent)

    def test_summarize_parallel_tool_calls(self):
        """Parallel tool_calls: naive split between tool(tc1) and tool(tc2)."""
        from core.context.compaction import _summarize_old_turns

        msgs = [
            _sys("sys"),                         # 0
            _user("q1"),                         # 1
            _assistant("a1"),                    # 2
            _assistant_tc(["tc1", "tc2"]),       # 3
            _tool("tc1"),                        # 4
            _tool("tc2"),                        # 5  ← naive cutoff for preserve_recent=4
            _assistant("a2"),                    # 6
            _user("q2"),                         # 7
            _assistant("a3"),                    # 8
        ]
        result = _summarize_old_turns(msgs, preserve_recent=4)
        recent = result[2:]
        _validate_no_orphaned_tools(recent)


# ── compact() end-to-end ─────────────────────────────────────────────

class TestCompactToolCallSafety:
    """Full compact() pipeline must not orphan tool messages."""

    def test_compact_preserves_tool_call_groups(self):
        from core.context.compaction import compact

        msgs = [_sys("sys")]
        # Build enough turns to trigger phase 2 (summarization)
        for i in range(10):
            msgs.append(_user(f"question {i} " + "x" * 200))
            msgs.append(_assistant_tc([f"tc_{i}"]))
            msgs.append(_tool(f"tc_{i}", f"result {i} " + "y" * 200))
            msgs.append(_assistant(f"answer {i} " + "z" * 200))

        result = compact(msgs, 500, preserve_recent=6)

        # Find the boundary between summary and recent
        recent_start = 0
        for j, m in enumerate(result):
            if "[Compacted" in (m.get("content") or ""):
                recent_start = j + 1
                break

        recent = result[recent_start:]
        _validate_no_orphaned_tools(recent)
