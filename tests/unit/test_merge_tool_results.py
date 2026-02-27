"""Tests for merge_tool_results_into_history — the unified merge+heal function.

Covers all edge-cloud failure scenarios:
  1. Edge disconnects, never sends tool_results
  2. Edge sends partial tool_results
  3. Cloud restarts, edge sends tool_results normally
  4. Cloud restarts, edge sends partial tool_results
  5. Cloud restarts, edge already gave up (new user message, no results)
  6. DB has trailing tool_calls with no tool_result (recovered)
  7. tool_results for unknown tool_call_ids (stale/wrong IDs)
  8. Multiple tool_calls in one assistant message, mixed results
  9. Multiple assistant messages with tool_calls across history
  10. Normal in-memory path (no recovery, tool_results follow naturally)
  11. Placeholder replacement (edge disconnects → heal → edge reconnects)
"""

from core.history_utils import merge_tool_results_into_history


def _sys():
    return {"role": "system", "content": "system"}


def _user(content="hello"):
    return {"role": "user", "content": content}


def _assistant_text(content="ok"):
    return {"role": "assistant", "content": content}


def _assistant_tc(*tc_ids):
    """Assistant message with tool_calls."""
    return {
        "role": "assistant",
        "content": "",
        "tool_calls": [
            {"id": tid, "type": "function",
             "function": {"name": f"tool_{tid}", "arguments": "{}"}}
            for tid in tc_ids
        ],
    }


def _tool(tc_id, content="result"):
    return {"role": "tool", "tool_call_id": tc_id, "content": content}


def _tr(tc_id, result="real result"):
    """Incoming tool_result from edge."""
    return {"tool_call_id": tc_id, "name": f"tool_{tc_id}", "result": result}


def _placeholder(tc_id):
    return {"role": "tool", "tool_call_id": tc_id,
            "content": "[not executed -- edge disconnected]"}


def _validate_sequence(history):
    """Assert the message sequence is valid for OpenAI-compatible APIs.

    Rules:
    - Every assistant message with tool_calls must be immediately followed
      by tool messages for ALL tool_call_ids (before any non-tool message).
    - No tool message without a preceding assistant tool_calls containing its ID.
    """
    for i, msg in enumerate(history):
        if msg.get("role") == "assistant" and msg.get("tool_calls"):
            expected = {tc["id"] for tc in msg["tool_calls"]}
            found = set()
            for j in range(i + 1, len(history)):
                if history[j].get("role") == "tool":
                    found.add(history[j].get("tool_call_id", ""))
                else:
                    break
            assert expected == found, (
                f"Invalid sequence at index {i}: "
                f"expected tool_call_ids {expected}, found {found}, "
                f"missing {expected - found}, extra {found - expected}"
            )


# ============================================================================
# Scenario 1: Edge disconnects, never sends tool_results
# ============================================================================

class TestEdgeDisconnect:
    def test_all_tool_calls_healed(self):
        """No tool_results at all → all get placeholders."""
        history = [_sys(), _user(), _assistant_tc("tc1", "tc2")]
        consumed = merge_tool_results_into_history(history, None)
        assert consumed == set()
        _validate_sequence(history)
        assert history[3] == _placeholder("tc1")
        assert history[4] == _placeholder("tc2")

    def test_empty_tool_results_list(self):
        """Empty list same as None."""
        history = [_sys(), _user(), _assistant_tc("tc1")]
        consumed = merge_tool_results_into_history(history, [])
        assert consumed == set()
        _validate_sequence(history)
        assert history[3] == _placeholder("tc1")


# ============================================================================
# Scenario 2: Edge sends partial tool_results
# ============================================================================

class TestPartialResults:
    def test_one_of_two_results(self):
        """Edge sends result for tc1 but not tc2."""
        history = [_sys(), _user(), _assistant_tc("tc1", "tc2")]
        consumed = merge_tool_results_into_history(history, [_tr("tc1")])
        assert consumed == {"tc1"}
        _validate_sequence(history)
        # tc1 has real result, tc2 has placeholder
        tool_msgs = [m for m in history if m["role"] == "tool"]
        assert len(tool_msgs) == 2
        tc1_msg = next(m for m in tool_msgs if m["tool_call_id"] == "tc1")
        tc2_msg = next(m for m in tool_msgs if m["tool_call_id"] == "tc2")
        assert tc1_msg["content"] == "real result"
        assert "[not executed" in tc2_msg["content"]

    def test_partial_with_existing_results(self):
        """History already has tc1 result, edge sends tc2."""
        history = [_sys(), _user(), _assistant_tc("tc1", "tc2"), _tool("tc1", "existing")]
        consumed = merge_tool_results_into_history(history, [_tr("tc2", "new")])
        assert consumed == {"tc2"}
        _validate_sequence(history)
        tool_msgs = [m for m in history if m["role"] == "tool"]
        assert len(tool_msgs) == 2


# ============================================================================
# Scenario 3: Cloud restarts, edge sends tool_results normally
# (THE BUG THAT TRIGGERED THIS REWRITE)
# ============================================================================

class TestCloudRestart:
    def test_results_merged_into_correct_position(self):
        """Cloud recovered history with orphaned tool_calls.
        Edge sends all tool_results. Results should be merged into the
        correct position (after assistant), not appended at end."""
        history = [
            _sys(),
            _user("read file"),
            _assistant_tc("tc1"),
            # No tool message — cloud just recovered from DB
        ]
        consumed = merge_tool_results_into_history(
            history, [_tr("tc1", "file contents")]
        )
        assert consumed == {"tc1"}
        _validate_sequence(history)
        # Result should be right after assistant, not at end
        assert history[3]["role"] == "tool"
        assert history[3]["content"] == "file contents"
        assert len(history) == 4

    def test_multiple_results_after_restart(self):
        """Cloud recovered, edge sends results for multiple tool_calls."""
        history = [
            _sys(),
            _user(),
            _assistant_tc("tc1", "tc2"),
        ]
        consumed = merge_tool_results_into_history(
            history, [_tr("tc1", "r1"), _tr("tc2", "r2")]
        )
        assert consumed == {"tc1", "tc2"}
        _validate_sequence(history)
        assert len(history) == 5  # sys + user + assistant + 2 tools


# ============================================================================
# Scenario 4: Cloud restarts, edge sends partial tool_results
# ============================================================================

class TestCloudRestartPartial:
    def test_partial_merge_plus_heal(self):
        """Cloud recovered with 3 orphaned tool_calls.
        Edge sends results for 2 of 3. Third gets placeholder."""
        history = [_sys(), _user(), _assistant_tc("tc1", "tc2", "tc3")]
        consumed = merge_tool_results_into_history(
            history, [_tr("tc1", "r1"), _tr("tc3", "r3")]
        )
        assert consumed == {"tc1", "tc3"}
        _validate_sequence(history)
        tool_msgs = {m["tool_call_id"]: m for m in history if m["role"] == "tool"}
        assert tool_msgs["tc1"]["content"] == "r1"
        assert "[not executed" in tool_msgs["tc2"]["content"]
        assert tool_msgs["tc3"]["content"] == "r3"


# ============================================================================
# Scenario 5: Cloud restarts, edge already gave up
# ============================================================================

class TestCloudRestartEdgeGaveUp:
    def test_no_results_all_healed(self):
        """Edge hit max-turns, sends new user message without tool_results."""
        history = [_sys(), _user("read file"), _assistant_tc("tc1")]
        consumed = merge_tool_results_into_history(history, None)
        assert consumed == set()
        _validate_sequence(history)
        assert history[3] == _placeholder("tc1")


# ============================================================================
# Scenario 6: DB trailing tool_calls (API crashed mid-execution)
# ============================================================================

class TestTrailingToolCalls:
    def test_trailing_tool_calls_healed(self):
        """_append_recovered_events flushed trailing tool_calls into an
        assistant message. Merge should heal them."""
        history = [
            _sys(),
            _user(),
            _assistant_tc("tc1"),  # flushed by _append_recovered_events
        ]
        consumed = merge_tool_results_into_history(history, None)
        _validate_sequence(history)
        assert history[3] == _placeholder("tc1")

    def test_trailing_tool_calls_with_late_results(self):
        """API crashed, but edge retries and sends results."""
        history = [_sys(), _user(), _assistant_tc("tc1")]
        consumed = merge_tool_results_into_history(
            history, [_tr("tc1", "late result")]
        )
        assert consumed == {"tc1"}
        _validate_sequence(history)
        assert history[3]["content"] == "late result"


# ============================================================================
# Scenario 7: tool_results for unknown tool_call_ids
# ============================================================================

class TestUnknownToolCallIds:
    def test_unknown_ids_not_consumed(self):
        """tool_results with IDs not in any assistant message are not consumed."""
        history = [_sys(), _user(), _assistant_tc("tc1")]
        consumed = merge_tool_results_into_history(
            history, [_tr("tc1", "r1"), _tr("tc_unknown", "stale")]
        )
        assert consumed == {"tc1"}
        assert "tc_unknown" not in consumed
        _validate_sequence(history)

    def test_all_unknown(self):
        """All tool_results have unknown IDs."""
        history = [_sys(), _user(), _assistant_text("no tools")]
        consumed = merge_tool_results_into_history(
            history, [_tr("tc_ghost", "phantom")]
        )
        assert consumed == set()
        _validate_sequence(history)


# ============================================================================
# Scenario 8: Multiple tool_calls, mixed existing and incoming
# ============================================================================

class TestMixedExistingAndIncoming:
    def test_some_already_in_history(self):
        """History has result for tc1, edge sends tc2. tc1 not duplicated."""
        history = [
            _sys(), _user(),
            _assistant_tc("tc1", "tc2"),
            _tool("tc1", "already here"),
        ]
        consumed = merge_tool_results_into_history(
            history, [_tr("tc1", "duplicate"), _tr("tc2", "new")]
        )
        # tc1 already exists → not consumed by merge (already in history)
        # tc2 is new → consumed
        assert "tc2" in consumed
        _validate_sequence(history)
        # tc1 should NOT be duplicated
        tc1_msgs = [m for m in history if m.get("tool_call_id") == "tc1"]
        assert len(tc1_msgs) == 1
        assert tc1_msgs[0]["content"] == "already here"


# ============================================================================
# Scenario 9: Multiple assistant messages with tool_calls
# ============================================================================

class TestMultipleAssistantToolCalls:
    def test_results_go_to_correct_assistant(self):
        """Two assistant messages with tool_calls in history.
        Results should be merged into the correct positions."""
        history = [
            _sys(),
            _user("step 1"),
            _assistant_tc("tc1"),
            _tool("tc1", "result 1"),
            _assistant_text("ok, now step 2"),
            _user("step 2"),
            _assistant_tc("tc2"),
            # tc2 has no result (cloud restarted here)
        ]
        consumed = merge_tool_results_into_history(
            history, [_tr("tc2", "result 2")]
        )
        assert consumed == {"tc2"}
        _validate_sequence(history)
        # tc2 result should be right after its assistant message
        assert history[7]["role"] == "tool"
        assert history[7]["tool_call_id"] == "tc2"
        assert history[7]["content"] == "result 2"

    def test_both_assistants_orphaned(self):
        """Two assistant messages, both orphaned. Edge sends results for second only."""
        history = [
            _sys(),
            _user(),
            _assistant_tc("tc1"),
            # no result for tc1
            _user("continue"),
            _assistant_tc("tc2"),
            # no result for tc2
        ]
        consumed = merge_tool_results_into_history(
            history, [_tr("tc2", "r2")]
        )
        assert consumed == {"tc2"}
        _validate_sequence(history)
        # tc1 should be healed, tc2 should have real result
        tool_msgs = {m["tool_call_id"]: m for m in history if m["role"] == "tool"}
        assert "[not executed" in tool_msgs["tc1"]["content"]
        assert tool_msgs["tc2"]["content"] == "r2"


# ============================================================================
# Scenario 10: Normal in-memory path (no recovery needed)
# ============================================================================

class TestNormalPath:
    def test_no_orphans_no_results(self):
        """Clean history, no tool_results → no changes."""
        history = [
            _sys(), _user(),
            _assistant_tc("tc1"), _tool("tc1", "done"),
            _assistant_text("all good"),
        ]
        original_len = len(history)
        consumed = merge_tool_results_into_history(history, None)
        assert consumed == set()
        assert len(history) == original_len
        _validate_sequence(history)

    def test_already_complete_history(self):
        """All tool_calls already have results. Incoming results are duplicates."""
        history = [
            _sys(), _user(),
            _assistant_tc("tc1"), _tool("tc1", "existing"),
        ]
        consumed = merge_tool_results_into_history(
            history, [_tr("tc1", "duplicate")]
        )
        # tc1 is consumed (so it won't be appended again) but content unchanged
        assert "tc1" in consumed
        _validate_sequence(history)
        # Should not duplicate
        tc1_msgs = [m for m in history if m.get("tool_call_id") == "tc1"]
        assert len(tc1_msgs) == 1
        assert tc1_msgs[0]["content"] == "existing"  # original preserved


# ============================================================================
# Edge cases
# ============================================================================

class TestEdgeCases:
    def test_empty_history(self):
        """Empty history with tool_results → results not consumed (no assistant)."""
        history = []
        consumed = merge_tool_results_into_history(history, [_tr("tc1")])
        assert consumed == set()
        assert len(history) == 0

    def test_system_only_history(self):
        """Only system message."""
        history = [_sys()]
        consumed = merge_tool_results_into_history(history, [_tr("tc1")])
        assert consumed == set()
        assert len(history) == 1

    def test_duplicate_tool_call_ids_in_results(self):
        """Edge sends same tool_call_id twice. First wins."""
        history = [_sys(), _user(), _assistant_tc("tc1")]
        consumed = merge_tool_results_into_history(
            history, [_tr("tc1", "first"), _tr("tc1", "second")]
        )
        assert consumed == {"tc1"}
        _validate_sequence(history)
        tc1_msgs = [m for m in history if m.get("tool_call_id") == "tc1"]
        assert len(tc1_msgs) == 1
        assert tc1_msgs[0]["content"] == "first"

    def test_tool_result_with_empty_content(self):
        """tool_result with empty string content."""
        history = [_sys(), _user(), _assistant_tc("tc1")]
        consumed = merge_tool_results_into_history(
            history, [{"tool_call_id": "tc1", "result": ""}]
        )
        assert consumed == {"tc1"}
        _validate_sequence(history)
        assert history[3]["content"] == ""

    def test_tool_result_missing_result_key(self):
        """tool_result without 'result' key defaults to empty string."""
        history = [_sys(), _user(), _assistant_tc("tc1")]
        consumed = merge_tool_results_into_history(
            history, [{"tool_call_id": "tc1"}]
        )
        assert consumed == {"tc1"}
        _validate_sequence(history)
        assert history[3]["content"] == ""


# ============================================================================
# Scenario 11: Placeholder replacement (edge disconnects → heal → reconnect)
# ============================================================================

class TestPlaceholderReplacement:
    """Edge disconnects → heal inserts placeholder → edge reconnects with real results."""

    def test_placeholder_replaced_by_real_result(self):
        """Placeholder must be replaced in-place, not duplicated."""
        history = [_sys(), _user(), _assistant_tc("tc1"), _placeholder("tc1")]
        consumed = merge_tool_results_into_history(history, [_tr("tc1", "real data")])
        assert "tc1" in consumed
        _validate_sequence(history)
        tc1_msgs = [m for m in history if m.get("tool_call_id") == "tc1"]
        assert len(tc1_msgs) == 1
        assert tc1_msgs[0]["content"] == "real data"

    def test_placeholder_replaced_multiple_tools(self):
        """Two healed placeholders, both get real results."""
        history = [
            _sys(), _user(), _assistant_tc("tc1", "tc2"),
            _placeholder("tc1"), _placeholder("tc2"),
        ]
        consumed = merge_tool_results_into_history(
            history, [_tr("tc1", "r1"), _tr("tc2", "r2")]
        )
        assert consumed == {"tc1", "tc2"}
        _validate_sequence(history)
        tool_msgs = {m["tool_call_id"]: m for m in history if m["role"] == "tool"}
        assert tool_msgs["tc1"]["content"] == "r1"
        assert tool_msgs["tc2"]["content"] == "r2"

    def test_placeholder_partial_replacement(self):
        """Two healed, only one gets real result. Other stays placeholder."""
        history = [
            _sys(), _user(), _assistant_tc("tc1", "tc2"),
            _placeholder("tc1"), _placeholder("tc2"),
        ]
        consumed = merge_tool_results_into_history(history, [_tr("tc1", "real")])
        assert "tc1" in consumed
        _validate_sequence(history)
        tool_msgs = {m["tool_call_id"]: m for m in history if m["role"] == "tool"}
        assert tool_msgs["tc1"]["content"] == "real"
        assert "[not executed" in tool_msgs["tc2"]["content"]

    def test_multi_turn_placeholder_then_real(self):
        """Turn 1: tool_calls → Turn 2: user msg (heals) → Turn 3: real results."""
        history = [
            _sys(), _user("read file"), _assistant_tc("tc1"),
            _placeholder("tc1"), _user("continue"),
        ]
        consumed = merge_tool_results_into_history(
            history, [_tr("tc1", "file contents")]
        )
        assert "tc1" in consumed
        _validate_sequence(history)
        tc1_msgs = [m for m in history if m.get("tool_call_id") == "tc1"]
        assert len(tc1_msgs) == 1
        assert tc1_msgs[0]["content"] == "file contents"
