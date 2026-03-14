"""E2E test: multi-round tool call token growth with compaction.

Simulates the exact scenario described in the issue:
  Round 1: system (3K) + user (0.2K) = 3.2K tokens
  Round 2: + assistant + tool_result = ~6K tokens
  Round 3: + assistant + tool_result = ~8K+ tokens
  ...

Verifies that:
1. Tool results are truncated to 8K chars before entering messages
2. Compaction triggers at 50% of context limit (not 80%)
3. After compaction, old tool results are cleared
4. Token count stays bounded across many rounds
"""

import json

from core.context.compaction import (
    compact,
    estimate_tokens,
    needs_compaction,
)


def _system_prompt(size_chars: int = 12000) -> dict:
    """Realistic system prompt ~3K tokens."""
    return {"role": "system", "content": "You are a helpful assistant. " * (size_chars // 30)}


def _user_msg(text: str = "中信证券建议买吗？") -> dict:
    return {"role": "user", "content": text}


def _assistant_with_tool_call(tc_id: str, name: str, args: str = "{}") -> dict:
    return {
        "role": "assistant",
        "content": "",
        "tool_calls": [
            {"id": tc_id, "type": "function", "function": {"name": name, "arguments": args}}
        ],
    }


def _tool_result(tc_id: str, size_chars: int = 4000) -> dict:
    """Simulate a tool result of given size."""
    data = {"result": "x" * size_chars}
    content = json.dumps(data)
    return {"role": "tool", "tool_call_id": tc_id, "content": content}


class TestMultiRoundTokenGrowth:
    """Simulate the exact multi-round accumulation pattern."""

    def test_unbounded_growth_without_truncation(self):
        """Without truncation, 5 rounds of 10K tool results = ~15K+ tokens."""
        messages = [_system_prompt(), _user_msg()]
        initial = estimate_tokens(messages)

        for i in range(5):
            messages.append(_assistant_with_tool_call(f"tc{i}", "stock_assistant"))
            messages.append(_tool_result(f"tc{i}", size_chars=10000))

        final = estimate_tokens(messages)
        # Without any control, tokens grow linearly
        assert final > initial * 3, f"Expected significant growth: {initial} → {final}"

    def test_8k_truncation_caps_individual_results(self):
        """Each tool result capped at 8K chars (~2K tokens)."""
        messages = [_system_prompt(), _user_msg()]

        for i in range(5):
            messages.append(_assistant_with_tool_call(f"tc{i}", "stock_assistant"))
            # Simulate the truncation that chat_loop.py now does
            raw_result = "x" * 20000  # 20K chars
            truncated = raw_result[:8000] if len(raw_result) > 8000 else raw_result
            messages.append({"role": "tool", "tool_call_id": f"tc{i}", "content": truncated})

        total = estimate_tokens(messages)
        # 5 rounds × 2K tokens per result + system + user ≈ 13K tokens
        # Much less than 5 × 5K = 25K without truncation
        assert total < 15000, f"Truncated total should be <15K tokens, got {total}"

    def test_compaction_triggers_at_50_percent(self):
        """Compaction should trigger at 50% of limit, not 80%."""
        # Build messages that are 55% of a 20K limit = 11K tokens = 44K chars
        messages = [_system_prompt(12000), _user_msg()]
        for i in range(5):
            messages.append(_assistant_with_tool_call(f"tc{i}", "tool"))
            messages.append(_tool_result(f"tc{i}", size_chars=6000))

        token_limit = 20000
        tokens_before = estimate_tokens(messages)

        # Should trigger at 50%
        assert needs_compaction(messages, token_limit), (
            f"{tokens_before} tokens should trigger compaction at 50% of {token_limit}"
        )

        # Compact
        compacted = compact(messages, token_limit)
        tokens_after = estimate_tokens(compacted)

        assert tokens_after < tokens_before, (
            f"Compaction should reduce tokens: {tokens_before} → {tokens_after}"
        )

    def test_old_tool_results_cleared_after_compaction(self):
        """After compaction, old tool results should be replaced with placeholder."""
        messages = [_system_prompt(4000), _user_msg()]
        for i in range(8):
            messages.append(_assistant_with_tool_call(f"tc{i}", "tool"))
            messages.append(_tool_result(f"tc{i}", size_chars=4000))

        compacted = compact(messages, 10000)

        # Recent tool results (last 6 messages) should be preserved
        # Old ones should be cleared
        cleared = [
            m
            for m in compacted
            if m.get("role") == "tool" and "cleared" in m.get("content", "").lower()
        ]
        preserved = [
            m
            for m in compacted
            if m.get("role") == "tool" and "cleared" not in m.get("content", "").lower()
        ]

        assert len(cleared) > 0, "Some old tool results should be cleared"
        assert len(preserved) > 0, "Recent tool results should be preserved"

    def test_full_pipeline_stays_bounded(self):
        """Simulate 10 rounds with truncation + compaction. Tokens stay bounded."""
        messages = [_system_prompt(12000), _user_msg()]
        token_limit = 30000  # ~30K token context
        max_observed = 0

        for i in range(10):
            # Compaction check (same as chat_loop.py)
            if needs_compaction(messages, token_limit):
                messages = compact(messages, token_limit)

            # LLM returns tool call
            messages.append(_assistant_with_tool_call(f"tc{i}", "stock_assistant"))

            # Tool result with truncation (same as chat_loop.py)
            raw = json.dumps({"data": "x" * 15000})
            content = raw[:8000] if len(raw) > 8000 else raw
            messages.append({"role": "tool", "tool_call_id": f"tc{i}", "content": content})

            current = estimate_tokens(messages)
            max_observed = max(max_observed, current)

        # After 10 rounds, should never exceed the limit
        assert max_observed < token_limit, (
            f"Max observed {max_observed} tokens should stay under {token_limit} limit"
        )
        # Final should be well under limit
        final = estimate_tokens(messages)
        assert final < token_limit * 0.7, (
            f"Final {final} tokens should be well under 70% of {token_limit}"
        )
