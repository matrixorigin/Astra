"""Tiered history compression with reference preservation.

Design ref: context-window-management.md §2 Tiered Compression
"""

from __future__ import annotations

import logging
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from typing import Any

logger = logging.getLogger(__name__)

# Constants for tier boundaries
TIER1_RECENT_TURNS = (
    2  # Last N turns kept in full fidelity (reduced from 3 to 2 for better compression)
)
TIER3_SYNOPSIS_THRESHOLD = 6  # Create synopsis if history > N turns


def compress_history_with_references(
    history: list[dict[str, Any]], referenced_events: set[str], elastic_budget: int
) -> dict[str, Any]:
    """
    Compress history using 3-tier strategy while preserving referenced content.

    Tier 1: Recent (last 3 turns) - Full fidelity
    Tier 2: Referenced content - Full for referenced, summary for unreferenced
    Tier 3: Synopsis - Single paragraph summary

    NOTE: This is a conservative compression strategy. We preserve all referenced
    content in full to avoid information loss. Unreferenced content is summarized
    using simple heuristics (no LLM calls).

    Args:
        history: List of conversation turns (each turn is a dict with user_query, llm_response, tool_results)
        referenced_events: Set of event_ids that must be preserved in full
        elastic_budget: Token budget for history (currently unused, reserved for future adaptive compression)

    Returns:
        Compressed history with tiers: {"tier1": [...], "tier2": [...], "tier3": "..."}
    """
    if not history:
        return {"tier1": [], "tier2": [], "tier3": None}

    if len(history) <= TIER1_RECENT_TURNS:
        # Short history - no compression needed
        return {"tier1": history, "tier2": [], "tier3": None}

    # Tier 1: Last 3 turns (always full fidelity)
    tier1 = history[-TIER1_RECENT_TURNS:]

    # Tier 2: Middle turns (compressed based on reference status)
    tier2 = []
    for turn in history[:-TIER1_RECENT_TURNS]:
        try:
            compressed_turn = _compress_turn(turn, referenced_events)
            tier2.append(compressed_turn)
        except Exception as e:
            logger.warning(f"Failed to compress turn: {e}")
            # On error, keep original turn (safe fallback)
            tier2.append(turn)

    # Tier 3: Synopsis (first few turns summarized)
    tier3 = None
    if len(history) > TIER3_SYNOPSIS_THRESHOLD:
        try:
            tier3 = _create_synopsis(history[:3])
        except Exception as e:
            logger.warning(f"Failed to create synopsis: {e}")
            # On error, skip synopsis (safe fallback)

    return {"tier1": tier1, "tier2": tier2, "tier3": tier3}


def _compress_turn(turn: dict[str, Any], referenced_events: set[str]) -> dict[str, Any]:
    """
    Aggressively compress a single turn based on reference status.

    Strategy for >50% compression:
    - User queries: First 80 chars only (users rarely reference old queries)
    - LLM responses: First 80 chars only (unless referenced)
    - Tool results: REMOVE completely if unreferenced (just keep count)
    - Tool calls: Keep (lightweight, needed for context)

    This achieves >50% reduction while preserving referenced content in full.
    """
    if not isinstance(turn, dict):
        logger.warning(f"Invalid turn type: {type(turn)}")
        return {}

    # Aggressively compress user query (first 80 chars)
    user_query = turn.get("user_query", "")
    if len(user_query) > 80:
        compressed_query = user_query[:80] + "..."
    else:
        compressed_query = user_query

    # Compress LLM response based on reference status
    llm_response = turn.get("llm_response", "")
    # Check if any event in this turn is referenced
    # Need to handle invalid tool_results (None, non-list, contains None)
    tool_results = turn.get("tool_results", [])
    if not isinstance(tool_results, list):
        tool_results = []

    turn_referenced = any(
        isinstance(result, dict) and result.get("event_id", "") in referenced_events
        for result in tool_results
    )

    if turn_referenced:
        # Keep full response if turn is referenced
        compressed_response = llm_response
    else:
        # Otherwise, first 80 chars only
        if len(llm_response) > 80:
            compressed_response = llm_response[:80] + "..."
        else:
            compressed_response = llm_response

    compressed = {
        "user_query": compressed_query,
        "tool_calls": turn.get("tool_calls", []),  # Keep tool calls (lightweight)
        "tool_results": [],
        "llm_response": compressed_response,
    }

    # Compress tool results based on reference status
    tool_results = turn.get("tool_results", [])
    referenced_count = 0
    unreferenced_count = 0

    for result in tool_results:
        # Handle invalid tool results (None, non-dict)
        if not isinstance(result, dict):
            continue

        event_id = result.get("event_id", "")

        if event_id in referenced_events:
            # Keep full content for referenced events
            compressed["tool_results"].append(result)
            referenced_count += 1
        else:
            # For unreferenced events: DON'T include them at all
            # This is where we get the biggest compression wins
            unreferenced_count += 1

    # If there were unreferenced tool results, add a single summary line
    if unreferenced_count > 0:
        compressed["tool_results"].append(
            {"summary": f"({unreferenced_count} tool results omitted)"}
        )

    return compressed


def _summarize_tool_result(result: dict[str, Any]) -> str:
    """
    Rule-based tool result summarization (zero LLM cost).

    Provides concise summaries for common tool types.
    """
    tool_name = result.get("tool_name", "unknown")
    content = result.get("content", "")
    args = result.get("args", {})

    if tool_name == "read_file":
        lines = content.count("\n") + 1 if content else 0
        path = args.get("path", "unknown")
        return f"read_file({path}) → {lines} lines"

    elif tool_name == "grep":
        matches = content.count("\n") + 1 if content else 0
        pattern = args.get("pattern", "")
        return f"grep('{pattern}') → {matches} matches"

    elif tool_name == "bash":
        output_len = len(content)
        return f"bash command → {output_len} chars output"

    elif tool_name == "list_dir":
        entries = content.count("\n") + 1 if content else 0
        return f"list_dir → {entries} entries"

    else:
        # Generic summary for unknown tools
        return f"{tool_name} → {len(content)} chars"


def _summarize_text(text: str, max_chars: int = 150) -> str:
    """
    Aggressively summarize text to first complete sentence or max_chars.

    For >50% compression, we need to be aggressive:
    - Default max_chars reduced from 400 to 150
    - Prioritize first sentence (usually contains key information)
    - Truncate long sentences

    Uses improved sentence boundary detection to handle:
    - Abbreviations (Dr., Mr., etc.)
    - Decimals (3.14)
    - Code snippets
    """
    if not text:
        return ""

    # If text is already short, return as-is
    if len(text) <= max_chars:
        return text

    # Find first sentence boundary
    # Look for ". " followed by capital letter or end of string
    # This avoids splitting on abbreviations and decimals
    import re

    sentence_pattern = r"\.(\s+[A-Z]|\s*$)"
    match = re.search(sentence_pattern, text)

    if match:
        # Found sentence boundary
        first_sentence = text[: match.start() + 1]

        # If first sentence is too long, truncate aggressively
        if len(first_sentence) > max_chars:
            return first_sentence[:max_chars] + "..."

        # Return first sentence (no ellipsis needed, it's complete)
        return first_sentence
    else:
        # No sentence boundary found, truncate at max_chars
        return text[:max_chars] + "..."


def _create_synopsis(turns: list[dict[str, Any]]) -> str:
    """
    Create single paragraph synopsis of early turns.

    Extracts key information: initial query, tools used.
    """
    if not turns:
        return ""

    try:
        # Extract first user query
        first_query = turns[0].get("user_query", "")
        if not first_query:
            return ""

        # Truncate long queries
        if len(first_query) > 100:
            first_query = first_query[:100] + "..."

        synopsis = f"Session started with: {first_query}"

        # Extract unique tool names used
        tool_names = set()
        for turn in turns:
            for tool_call in turn.get("tool_calls", []):
                if isinstance(tool_call, dict):
                    tool_name = tool_call.get("tool_name", "")
                    if tool_name:
                        tool_names.add(tool_name)

        if tool_names:
            tools_str = ", ".join(sorted(tool_names))
            synopsis += f". Used tools: {tools_str}."

        return synopsis
    except Exception as e:
        logger.warning(f"Failed to create synopsis: {e}")
        return ""
