"""Procedural memory conflict resolution and tool description injection.

P0 Critical: Zero-cost helpers to detect when user instructions contradict
procedural memory patterns, ensuring user intent always wins.

Tool injection design:
  - Matching: keyword overlap between memory content and tool name/description
    (no embedding cost at injection time; embeddings are pre-computed at store time)
  - Safety: injected hints are appended as a separate "Learned hints:" section,
    never modifying the original tool schema fields (name, parameters, etc.)
  - Cache safety: original tool schema is never mutated; a shallow copy is returned

Design ref: context-window-management.md §1 Priority Resolution Mechanism
"""

from __future__ import annotations

import re
from typing import Any


def extract_parameter_values(pattern: str) -> dict[str, str]:
    """
    Extract parameter=value pairs from procedural memory pattern.
    
    P0 Implementation: 2 lines of regex, zero LLM cost.
    
    Examples:
        >>> extract_parameter_values("use analysis_type='overview'")
        {'analysis_type': 'overview'}
        >>> extract_parameter_values("set period='3mo' and type='stock'")
        {'period': '3mo', 'type': 'stock'}
    
    Args:
        pattern: Procedural memory content string
        
    Returns:
        Dictionary of parameter names to values
    """
    matches = re.findall(r"(\w+)=['\"]?(\w+)['\"]?", pattern)
    return dict(matches)


def extract_user_specified_value(user_message: str, param: str) -> str | None:
    """
    Extract user-specified value for a parameter from user message.
    
    P0 Implementation: Simple keyword matching, zero LLM cost.
    
    Examples:
        >>> extract_user_specified_value("give me advice analysis", "analysis_type")
        'advice'
        >>> extract_user_specified_value("analyze this stock", "analysis_type")
        None
        >>> extract_user_specified_value("last 6 months data", "period")
        '6mo'
    
    Args:
        user_message: User's input message
        param: Parameter name to look for
        
    Returns:
        Extracted value if found, None otherwise
    """
    # Common parameter patterns
    patterns = {
        "analysis_type": r"(overview|advice|technical|trend|risk)",
        "period": r"(\d+)\s*(?:mo|month|year|day|week)s?",  # Match "6 months", "3mo", etc.
    }
    
    if param in patterns:
        match = re.search(patterns[param], user_message, re.IGNORECASE)
        if match:
            # For period, normalize to short form
            if param == "period":
                num = match.group(1)
                # Extract unit from the full match
                unit_match = re.search(r"(mo|month|year|day|week)", match.group(0), re.IGNORECASE)
                if unit_match:
                    unit = unit_match.group(1).lower()
                    # Normalize to short form
                    if unit.startswith("mo"):
                        return f"{num}mo"
                    elif unit.startswith("year"):
                        return f"{num}year"
                    elif unit.startswith("day"):
                        return f"{num}day"
                    elif unit.startswith("week"):
                        return f"{num}week"
            return match.group(1)
        return None
    
    # Generic: look for "param: value" or "param = value"
    match = re.search(rf"{param}[:\s=]+['\"]?(\w+)['\"]?", user_message, re.IGNORECASE)
    return match.group(1) if match else None


def contradicts_user_intent(pattern: str, user_message: str) -> bool:
    """
    Detect if user message explicitly contradicts a procedural pattern.
    
    P0 Critical: Ensures user instructions always override learned patterns.
    
    Examples:
        >>> contradicts_user_intent("use analysis_type='overview'", "give me advice analysis")
        True  # User wants 'advice', pattern says 'overview'
        >>> contradicts_user_intent("use analysis_type='overview'", "analyze this stock")
        False  # User didn't specify, no contradiction
    
    Args:
        pattern: Procedural memory pattern
        user_message: User's input message
        
    Returns:
        True if user explicitly contradicts the pattern, False otherwise
    """
    # Extract parameter values from pattern
    pattern_params = extract_parameter_values(pattern)
    
    # Check if user message specifies different values
    for param, learned_value in pattern_params.items():
        user_value = extract_user_specified_value(user_message, param)
        if user_value and user_value != learned_value:
            return True
    
    return False


# ── Tool description injection ────────────────────────────────────────────────

_STOPWORDS = frozenset({"use", "the", "a", "an", "and", "or", "to", "for", "with", "in", "of", "is"})
_MIN_KEYWORD_LEN = 3


def _keywords(text: str) -> set[str]:
    """Extract meaningful lowercase keywords from text."""
    return {
        w.lower() for w in re.findall(r"\w+", text)
        if len(w) >= _MIN_KEYWORD_LEN and w.lower() not in _STOPWORDS
    }


def _tool_matches_memory(tool: dict[str, Any], memory_content: str) -> bool:
    """Return True if the memory content is relevant to this tool.

    Matching strategy: keyword overlap between memory text and tool name +
    description. No embedding cost — embeddings are used at retrieval time,
    not at injection time.

    Rules:
    - At least 2 overlapping keywords.
    - At least one of those keywords must be longer than 4 chars, to prevent
      short generic words ("api", "get", "run") from triggering false matches.
    """
    tool_text = f"{tool.get('name', '')} {tool.get('description', '')}"
    tool_kw = _keywords(tool_text)
    mem_kw = _keywords(memory_content)
    overlap = tool_kw & mem_kw
    return len(overlap) >= 2 and any(len(w) > 4 for w in overlap)


def inject_procedural_hints(
    tools: list[dict[str, Any]],
    procedural_memories: list[str],
) -> list[dict[str, Any]]:
    """Append learned hints to matching tool descriptions.

    Safety guarantees:
    - Returns shallow copies; original tool dicts are never mutated.
    - Hints are appended as "Learned hints: ..." — never replacing existing fields.
    - Provider tool schema caches are unaffected (original objects unchanged).
    - If no memories match a tool, the original dict is returned as-is (no copy).

    Args:
        tools: List of tool schema dicts (name, description, parameters, ...).
        procedural_memories: List of procedural memory content strings.

    Returns:
        List of tool dicts, some with augmented descriptions.
    """
    if not procedural_memories:
        return tools

    result = []
    for tool in tools:
        matching_hints = [
            mem for mem in procedural_memories
            if _tool_matches_memory(tool, mem)
        ]
        if not matching_hints:
            result.append(tool)
        else:
            augmented = dict(tool)  # shallow copy — schema fields preserved
            hints_text = "; ".join(matching_hints[:3])  # cap at 3 hints per tool
            existing_desc = augmented.get("description", "")
            augmented["description"] = f"{existing_desc}\nLearned hints: {hints_text}"
            result.append(augmented)

    return result

