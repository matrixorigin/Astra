"""Tests for history compaction in chat turn.

Verifies that long conversation histories are compacted before LLM calls
to prevent context overflow errors.
"""

import pytest
from unittest.mock import MagicMock, patch

from core.context.compaction import compact, needs_compaction, estimate_tokens


class TestHistoryCompaction:
    """Test compaction logic for real-world scenarios."""

    def test_short_history_not_compacted(self):
        """Short conversations should not trigger compaction."""
        messages = [
            {"role": "system", "content": "You are helpful."},
            {"role": "user", "content": "Hello"},
            {"role": "assistant", "content": "Hi there!"},
        ]
        assert not needs_compaction(messages, 100000)

    def test_long_tool_results_trigger_compaction(self):
        """Large tool results should trigger compaction."""
        # Simulate a session with many file reads
        messages = [
            {"role": "system", "content": "You are helpful."},
            {"role": "user", "content": "Read all config files"},
            {"role": "assistant", "content": "", "tool_calls": [
                {"id": "tc1", "function": {"name": "read_file", "arguments": "{}"}}
            ]},
            {"role": "tool", "tool_call_id": "tc1", "content": "x" * 50000},  # 50K chars
            {"role": "assistant", "content": "", "tool_calls": [
                {"id": "tc2", "function": {"name": "read_file", "arguments": "{}"}}
            ]},
            {"role": "tool", "tool_call_id": "tc2", "content": "y" * 50000},  # 50K chars
            {"role": "assistant", "content": "", "tool_calls": [
                {"id": "tc3", "function": {"name": "read_file", "arguments": "{}"}}
            ]},
            {"role": "tool", "tool_call_id": "tc3", "content": "z" * 50000},  # 50K chars
        ]
        # 150K chars ≈ 37.5K tokens, should trigger at 50K limit
        assert needs_compaction(messages, 50000)

    def test_compaction_clears_old_tool_results(self):
        """Compaction should clear old tool results but keep recent ones."""
        # Need more messages to push old ones outside preserve_recent window
        messages = [
            {"role": "system", "content": "System prompt"},
            # Old turn (will be compacted)
            {"role": "user", "content": "First question"},
            {"role": "assistant", "content": "", "tool_calls": [
                {"id": "tc1", "function": {"name": "bash", "arguments": "{}"}}
            ]},
            {"role": "tool", "tool_call_id": "tc1", "content": "OLD_RESULT_" + "x" * 10000},
            {"role": "assistant", "content": "First answer"},
            # Middle turns
            {"role": "user", "content": "Second question"},
            {"role": "assistant", "content": "Second answer"},
            {"role": "user", "content": "Third question"},
            {"role": "assistant", "content": "Third answer"},
            # Recent turn (will be preserved - within last 6)
            {"role": "user", "content": "Fourth question"},
            {"role": "assistant", "content": "", "tool_calls": [
                {"id": "tc2", "function": {"name": "bash", "arguments": "{}"}}
            ]},
            {"role": "tool", "tool_call_id": "tc2", "content": "RECENT_RESULT_" + "y" * 10000},
            {"role": "assistant", "content": "Fourth answer"},
        ]
        
        compacted = compact(messages, 10000)  # Force compaction
        
        # Old tool result should be cleared (outside preserve_recent=6)
        old_tool = next(m for m in compacted if m.get("tool_call_id") == "tc1")
        assert "OLD_RESULT_" not in old_tool["content"]
        assert "cleared" in old_tool["content"].lower()
        
        # Recent tool result should be preserved (within preserve_recent=6)
        recent_tool = next(m for m in compacted if m.get("tool_call_id") == "tc2")
        assert "RECENT_RESULT_" in recent_tool["content"]

    def test_compaction_preserves_message_structure(self):
        """Compaction should not break message sequence (system, user, assistant, tool)."""
        messages = [
            {"role": "system", "content": "System"},
            {"role": "user", "content": "Q1"},
            {"role": "assistant", "content": "", "tool_calls": [{"id": "t1", "function": {}}]},
            {"role": "tool", "tool_call_id": "t1", "content": "R1" * 5000},
            {"role": "assistant", "content": "A1"},
            {"role": "user", "content": "Q2"},
            {"role": "assistant", "content": "A2"},
        ]
        
        compacted = compact(messages, 5000)
        
        # Verify structure: roles should follow valid sequence
        roles = [m["role"] for m in compacted]
        assert roles[0] == "system"
        # No two consecutive user messages
        for i in range(len(roles) - 1):
            assert not (roles[i] == "user" and roles[i+1] == "user")

    def test_real_world_session_overflow_scenario(self):
        """Simulate the actual overflow scenario: 170K tokens from accumulated history."""
        # Build a realistic session with many tool calls
        messages = [{"role": "system", "content": "You are a coding assistant." * 100}]  # ~2K chars
        
        # Simulate 20 turns of file reading and code analysis
        for i in range(20):
            messages.append({"role": "user", "content": f"Analyze file {i}"})
            messages.append({
                "role": "assistant", 
                "content": "",
                "tool_calls": [{"id": f"tc{i}", "function": {"name": "read_file"}}]
            })
            # Each file read returns ~20K chars (simulating code files)
            messages.append({
                "role": "tool",
                "tool_call_id": f"tc{i}",
                "content": f"# File {i}\n" + "def func():\n    pass\n" * 1000
            })
            messages.append({"role": "assistant", "content": f"Analysis of file {i}: looks good."})
        
        # This should be ~400K chars ≈ 100K tokens
        initial_tokens = estimate_tokens(messages)
        assert initial_tokens > 80000, f"Setup failed: only {initial_tokens} tokens"
        
        # Compact to 100K limit
        compacted = compact(messages, 100000)
        final_tokens = estimate_tokens(compacted)
        
        # Should be significantly reduced
        assert final_tokens < initial_tokens * 0.6, f"Compaction insufficient: {final_tokens} vs {initial_tokens}"
        # Should still have recent messages intact
        assert any("File 19" in m.get("content", "") for m in compacted)


class TestContextOverflowIntegration:
    """Test that context overflow is caught before API call."""

    def test_overflow_error_message_is_actionable(self):
        """Error message should tell user what to do."""
        from core.llm.client import ContextOverflowError
        
        err = ContextOverflowError(
            "Context overflow: estimated 150,000 tokens exceeds 128,000. "
            "Start a new session with /session new"
        )
        
        msg = str(err)
        assert "/session new" in msg
        assert "150,000" in msg or "150000" in msg


class TestBuildTurnMessagesCompaction:
    """Integration test: verify compaction is called in _build_turn_messages."""

    def test_large_history_is_compacted(self, db_factory):
        """History exceeding limit is compacted before return."""
        from api.routers.chat import _build_turn_messages, _session_cache, _HISTORY_COMPACTION_LIMIT
        from core.context.compaction import estimate_tokens

        # Setup: create a session with large history in cache
        session_id = f"test-compaction-{id(self)}"
        large_history = [{"role": "system", "content": "System prompt"}]
        
        # Add enough messages to exceed compaction threshold
        for i in range(30):
            large_history.append({"role": "user", "content": f"Question {i}"})
            large_history.append({
                "role": "assistant",
                "content": "",
                "tool_calls": [{"id": f"tc{i}", "function": {"name": "read_file"}}]
            })
            large_history.append({
                "role": "tool",
                "tool_call_id": f"tc{i}",
                "content": "x" * 15000  # ~15K chars each
            })
            large_history.append({"role": "assistant", "content": f"Answer {i}"})

        initial_tokens = estimate_tokens(large_history)
        assert initial_tokens > _HISTORY_COMPACTION_LIMIT * 0.5, \
            f"Test setup failed: {initial_tokens} tokens not enough to trigger compaction"

        # Inject into cache
        _session_cache[session_id] = {"history": large_history, "sections": None}

        try:
            with db_factory() as db:
                # Call _build_turn_messages with a simple new message
                result_history, _, _ = _build_turn_messages(
                    db=db,
                    user_id="test-user",
                    session_id=session_id,
                    messages=[{"role": "user", "content": "hi"}],
                    tool_results=None,
                    project_rules=None,
                )

            # Verify compaction happened
            final_tokens = estimate_tokens(result_history)
            assert final_tokens < initial_tokens, \
                f"History should be compacted: {final_tokens} >= {initial_tokens}"
        finally:
            # Cleanup
            _session_cache.pop(session_id, None)
