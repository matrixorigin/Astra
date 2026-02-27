"""Context compaction for long-horizon tasks.

Design ref: memory-architecture.md §2 "Compaction for Long-Horizon Tasks"

When the message chain approaches the context window limit:
1. Clear raw tool outputs deep in history (already processed)
2. Summarize old conversation turns, preserve recent ones
3. Preserve key decisions and unresolved issues

Distributed-safe: stateless — operates on the in-memory message list only.
"""

from __future__ import annotations

import logging
from typing import Any

logger = logging.getLogger(__name__)

# Rough token estimate: 1 token ≈ 4 chars
_CHARS_PER_TOKEN = 4

# How many recent messages to always preserve verbatim
_PRESERVE_RECENT = 6

# Placeholder for cleared tool results
_TOOL_CLEARED = "[tool output cleared — already processed]"


def estimate_tokens(messages: list[dict[str, Any]]) -> int:
    """Estimate total tokens in a message chain."""
    total = 0
    for m in messages:
        content = m.get("content") or ""
        total += len(content) // _CHARS_PER_TOKEN
        # tool_calls JSON adds overhead
        if m.get("tool_calls"):
            total += 50 * len(m["tool_calls"])
    return total


def needs_compaction(messages: list[dict[str, Any]], token_limit: int) -> bool:
    """Check if message chain needs compaction (>80% of limit)."""
    return estimate_tokens(messages) > int(token_limit * 0.8)


def compact(
    messages: list[dict[str, Any]],
    token_limit: int,
    llm_summarize=None,
    preserve_recent: int = _PRESERVE_RECENT,
) -> list[dict[str, Any]]:
    """Compact a message chain to fit within token limit.

    Strategy (applied in order until under budget):
    1. Clear tool result contents in old messages
    2. If still over: summarize old turns via LLM (or simple truncation)

    Args:
        messages: Full message chain [system, user, assistant, tool, ...]
        token_limit: Target token limit
        llm_summarize: Optional async callable(text) -> summary. If None, uses truncation.
        preserve_recent: Number of recent messages to keep verbatim

    Returns:
        Compacted message list (new list, original not mutated)
    """
    if not needs_compaction(messages, token_limit):
        return messages

    result = [m.copy() for m in messages]

    # Phase 1: Clear old tool results (keep recent ones)
    result = _clear_old_tool_results(result, preserve_recent)

    if estimate_tokens(result) <= int(token_limit * 0.8):
        logger.info("Compaction phase 1 sufficient (tool result clearing)")
        return result

    # Phase 2: Summarize old conversation turns
    result = _summarize_old_turns(result, preserve_recent, llm_summarize)
    logger.info(
        f"Compaction complete: {estimate_tokens(result)} tokens "
        f"(limit: {token_limit})"
    )
    return result


def _clear_old_tool_results(
    messages: list[dict[str, Any]], preserve_recent: int,
) -> list[dict[str, Any]]:
    """Replace old tool result contents with placeholder.
    
    Preserves [memory:xxx] references so LLM can still expand them.
    """
    import re
    
    if len(messages) <= preserve_recent:
        return messages

    cutoff = len(messages) - preserve_recent
    for i in range(cutoff):
        m = messages[i]
        if m.get("role") == "tool" and m.get("content") and len(m["content"]) > 200:
            content = m["content"]
            # Extract and preserve memory references
            memory_refs = re.findall(r'\[(?:Full output.*?)?memory:[^\]]+\]', content)
            if memory_refs:
                # Keep the reference, clear the rest
                preserved = "\n".join(memory_refs)
                messages[i] = {**m, "content": f"{_TOOL_CLEARED}\n{preserved}"}
            else:
                messages[i] = {**m, "content": _TOOL_CLEARED}
    return messages


def _summarize_old_turns(
    messages: list[dict[str, Any]],
    preserve_recent: int,
    llm_summarize=None,
) -> list[dict[str, Any]]:
    """Summarize old user/assistant turns into a single system message."""
    if len(messages) <= preserve_recent + 1:
        return messages

    # Split: system (first msg) | old turns | recent turns
    system_msg = messages[0] if messages[0].get("role") == "system" else None
    start = 1 if system_msg else 0
    cutoff = len(messages) - preserve_recent

    old_turns = messages[start:cutoff]
    recent = messages[cutoff:]

    if not old_turns:
        return messages

    # Build summary text from old turns
    summary_parts: list[str] = []
    for m in old_turns:
        role = m.get("role", "?")
        content = m.get("content") or ""
        if role in ("user", "assistant") and content:
            # Truncate long individual messages
            text = content[:500] + "..." if len(content) > 500 else content
            summary_parts.append(f"{role}: {text}")

    if llm_summarize:
        # LLM-based summarization (caller provides the function)
        raw = "\n".join(summary_parts)
        try:
            summary_text = llm_summarize(raw)
        except Exception:
            logger.warning("LLM summarization failed, using truncation")
            summary_text = _truncate_summary(summary_parts)
    else:
        summary_text = _truncate_summary(summary_parts)

    summary_msg = {
        "role": "system",
        "content": f"[Compacted conversation summary]\n{summary_text}",
    }

    result = []
    if system_msg:
        result.append(system_msg)
    result.append(summary_msg)
    result.extend(recent)
    return result


def _truncate_summary(parts: list[str], max_chars: int = 2000) -> str:
    """Simple truncation-based summary."""
    text = "\n".join(parts)
    if len(text) <= max_chars:
        return text
    return text[:max_chars] + "\n[...earlier conversation truncated]"
