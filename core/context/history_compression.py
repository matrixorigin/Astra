"""Tiered history compression with reference preservation.

Design ref: context-window-management.md §2 Tiered Compression
"""

from __future__ import annotations

from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from typing import Any


def compress_history_with_references(
    history: list[dict[str, Any]],
    referenced_events: set[str],
    elastic_budget: int
) -> dict[str, Any]:
    """
    Compress history using 3-tier strategy while preserving referenced content.
    
    Tier 1: Recent (last 3 turns) - Full fidelity
    Tier 2: Referenced content - Full for referenced, summary for unreferenced
    Tier 3: Synopsis - Single paragraph summary
    
    Args:
        history: List of conversation turns
        referenced_events: Set of event_ids that must be preserved
        elastic_budget: Token budget for history
        
    Returns:
        Compressed history with tiers
    """
    if len(history) <= 3:
        return {"tier1": history, "tier2": [], "tier3": None}
    
    # Tier 1: Last 3 turns (always full)
    tier1 = history[-3:]
    
    # Tier 2: Middle turns (4..N-3)
    tier2 = []
    for turn in history[:-3]:
        compressed_turn = _compress_turn(turn, referenced_events)
        tier2.append(compressed_turn)
    
    # Tier 3: Synopsis (first 3 turns)
    tier3 = _create_synopsis(history[:3]) if len(history) > 6 else None
    
    return {"tier1": tier1, "tier2": tier2, "tier3": tier3}


def _compress_turn(turn: dict[str, Any], referenced_events: set[str]) -> dict[str, Any]:
    """Compress a single turn based on reference status."""
    compressed = {
        "user_query": turn.get("user_query", ""),
        "tool_calls": [],
        "tool_results": [],
        "llm_response": _summarize_text(turn.get("llm_response", ""))
    }
    
    # Compress tool results
    for result in turn.get("tool_results", []):
        event_id = result.get("event_id", "")
        if event_id in referenced_events:
            # Keep full content
            compressed["tool_results"].append(result)
        else:
            # Summarize
            compressed["tool_results"].append({
                "event_id": event_id,
                "tool_name": result.get("tool_name", ""),
                "summary": _summarize_tool_result(result)
            })
    
    # Keep tool calls (lightweight)
    compressed["tool_calls"] = turn.get("tool_calls", [])
    
    return compressed


def _summarize_tool_result(result: dict[str, Any]) -> str:
    """Rule-based tool result summarization (zero LLM cost)."""
    tool_name = result.get("tool_name", "")
    content = result.get("content", "")
    args = result.get("args", {})
    
    if tool_name == "read_file":
        lines = content.count("\n") + 1
        return f"read_file({args.get('path', 'unknown')}) → {lines} lines"
    elif tool_name == "grep":
        matches = content.count("\n") + 1
        return f"grep('{args.get('pattern', '')}') → {matches} matches"
    elif tool_name == "bash":
        return f"bash command → {len(content)} chars output"
    else:
        return f"{tool_name} → {len(content)} chars"


def _summarize_text(text: str, max_tokens: int = 100) -> str:
    """Summarize text to first sentence."""
    if not text:
        return ""
    
    # First sentence
    sentences = text.split(". ")
    first = sentences[0] + ("." if not sentences[0].endswith(".") else "")
    
    # Truncate if too long
    if len(first) > max_tokens * 4:  # ~4 chars per token
        return first[:max_tokens * 4] + "..."
    
    return first + ("..." if len(sentences) > 1 else "")


def _create_synopsis(turns: list[dict[str, Any]]) -> str:
    """Create single paragraph synopsis of early turns."""
    if not turns:
        return ""
    
    # Extract key info
    user_queries = [t.get("user_query", "") for t in turns]
    tool_names = []
    for t in turns:
        tool_names.extend([tc.get("tool_name", "") for tc in t.get("tool_calls", [])])
    
    synopsis = f"Session started with: {user_queries[0][:100]}. "
    if tool_names:
        synopsis += f"Used tools: {', '.join(set(tool_names))}. "
    
    return synopsis
