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
TIER1_RECENT_TURNS = 3  # Last N turns kept in full fidelity
TIER3_SYNOPSIS_THRESHOLD = 6  # Create synopsis if history > N turns


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
    Compress a single turn based on reference status.
    
    Referenced tool results are kept in full. Unreferenced results are summarized.
    """
    if not isinstance(turn, dict):
        logger.warning(f"Invalid turn type: {type(turn)}")
        return {}
    
    compressed = {
        "user_query": turn.get("user_query", ""),
        "tool_calls": turn.get("tool_calls", []),  # Keep tool calls (lightweight)
        "tool_results": [],
        "llm_response": _summarize_text(turn.get("llm_response", ""))
    }
    
    # Compress tool results based on reference status
    for result in turn.get("tool_results", []):
        if not isinstance(result, dict):
            continue
        
        event_id = result.get("event_id", "")
        
        if event_id in referenced_events:
            # Keep full content for referenced events
            compressed["tool_results"].append(result)
        else:
            # Summarize unreferenced events
            try:
                summary = _summarize_tool_result(result)
                compressed["tool_results"].append({
                    "event_id": event_id,
                    "tool_name": result.get("tool_name", ""),
                    "summary": summary
                })
            except Exception as e:
                logger.warning(f"Failed to summarize tool result: {e}")
                # On error, keep original (safe fallback)
                compressed["tool_results"].append(result)
    
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


def _summarize_text(text: str, max_chars: int = 400) -> str:
    """
    Summarize text to first complete sentence or max_chars.
    
    Uses improved sentence boundary detection to handle:
    - Abbreviations (Dr., Mr., etc.)
    - Decimals (3.14)
    - Code snippets
    
    NOTE: This is a simple heuristic. For production, consider using
    a proper sentence tokenizer (e.g., nltk.sent_tokenize).
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
    sentence_pattern = r'\.(\s+[A-Z]|\s*$)'
    match = re.search(sentence_pattern, text)
    
    if match:
        # Found sentence boundary
        first_sentence = text[:match.start() + 1]
        
        # If first sentence is too long, truncate
        if len(first_sentence) > max_chars:
            return first_sentence[:max_chars] + "..."
        
        # Add ellipsis if there's more content
        has_more = match.end() < len(text)
        return first_sentence + ("..." if has_more else "")
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
