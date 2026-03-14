"""Reference tracking for history compression.

Uses lightweight heuristics to identify which prior tool results are referenced.
Design ref: context-window-management.md §2 Phase 2.5
"""

from __future__ import annotations

import re
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from typing import Any


def analyze_semantic_references(
    current_turn_response: str,
    current_turn_tool_calls: list[dict[str, Any]],
    history: list[dict[str, Any]],
) -> set[str]:
    """
    Identify which prior tool results are semantically referenced in current reasoning.

    Uses lightweight heuristics (not LLM-based):
    1. Explicit references: "As seen in config.py..." → marks read_file(config.py)
    2. Data references: Current response contains data from prior tool result
    3. Causal chain: Current tool call uses output from prior call

    NOTE: This is a conservative heuristic approach. False negatives (missing a reference)
    are acceptable - we'll just compress that content. False positives (marking unreferenced
    content) waste tokens but don't lose information.

    Args:
        current_turn_response: Current LLM response text
        current_turn_tool_calls: Tool calls made in current turn
        history: List of prior turns with tool calls and results

    Returns:
        Set of event_ids that must be preserved in full

    Example:
        >>> refs = analyze_semantic_references(
        ...     "In config.py, DATABASE_URL is set to...",
        ...     [],
        ...     [{"tool_calls": [{"tool_name": "read_file", "args": {"path": "config.py"}, "event_id": "evt_1"}]}]
        ... )
        >>> "evt_1" in refs
        True
    """
    if not current_turn_response:
        current_turn_response = ""

    referenced_events = set()

    # Validate history
    if not history or not isinstance(history, list):
        return referenced_events

    # Heuristic 1: Explicit file/tool mentions in LLM response
    for turn in history:
        # Skip invalid turns
        if not isinstance(turn, dict):
            continue

        for tool_call in turn.get("tool_calls", []) or []:
            if not isinstance(tool_call, dict):
                continue

            tool_name = tool_call.get("tool_name", "")
            args = tool_call.get("args", {})
            event_id = tool_call.get("event_id", "")

            if not event_id:
                continue

            if tool_name == "read_file":
                # Check if filename is mentioned (full path or basename)
                path = args.get("path", "") if isinstance(args, dict) else ""
                if path:
                    filename = path.split("/")[-1]
                    # Check both full path and filename
                    if path in current_turn_response or filename in current_turn_response:
                        referenced_events.add(event_id)

            elif tool_name == "grep":
                # Check if pattern is mentioned
                pattern = args.get("pattern", "") if isinstance(args, dict) else ""
                if pattern and pattern in current_turn_response:
                    referenced_events.add(event_id)

    # Heuristic 2: Data overlap (substring matching for structured data)
    for turn in history:
        if not isinstance(turn, dict):
            continue

        for tool_result in turn.get("tool_results", []):
            if not isinstance(tool_result, dict):
                continue

            content = tool_result.get("content", "")
            event_id = tool_result.get("event_id", "")

            if not event_id or not content:
                continue

            # Extract key identifiers (variable names, function names, etc.)
            key_data = _extract_key_identifiers(content)
            if any(kd in current_turn_response for kd in key_data):
                referenced_events.add(event_id)

    # Heuristic 3: Causal chain (tool output → tool input)
    # If current tool call arguments contain data from prior tool results
    if current_turn_tool_calls and isinstance(current_turn_tool_calls, list):
        for tool_call in current_turn_tool_calls:
            if not isinstance(tool_call, dict):
                continue

            args = tool_call.get("args", {})
            if not isinstance(args, dict):
                continue

            for turn in history:
                if not isinstance(turn, dict):
                    continue

                for tool_result in turn.get("tool_results", []):
                    if not isinstance(tool_result, dict):
                        continue

                    content = tool_result.get("content", "")
                    event_id = tool_result.get("event_id", "")

                    if not event_id or not content:
                        continue

                    # Check if any arg value appears in prior tool result
                    # This catches cases like: grep result → used in next grep pattern
                    for arg_val in args.values():
                        if arg_val and str(arg_val) in content:
                            referenced_events.add(event_id)
                            break

    return referenced_events


def _extract_key_identifiers(content: str) -> list[str]:
    """
    Extract key identifiers from tool result content.

    Looks for:
    - Variable names (e.g., DATABASE_URL, API_KEY)
    - Function names (e.g., def foo(), function bar())
    - Class names (e.g., class MyClass)

    Returns:
        List of up to 20 identifiers found in content
    """
    if not content:
        return []

    identifiers = []

    # Variable assignments (VAR = value, VAR: value)
    # Matches: DATABASE_URL = "...", API_KEY: "..."
    var_pattern = r"\b([A-Z_][A-Z0-9_]{2,})\s*[=:]"
    identifiers.extend(re.findall(var_pattern, content))

    # Function definitions
    # Matches: def foo(), function bar()
    func_pattern = r"def\s+(\w+)\s*\(|function\s+(\w+)\s*\("
    for match in re.finditer(func_pattern, content):
        identifiers.extend([g for g in match.groups() if g])

    # Class definitions
    # Matches: class MyClass
    class_pattern = r"class\s+(\w+)"
    identifiers.extend(re.findall(class_pattern, content))

    # Limit to top 20 to avoid performance issues
    return identifiers[:20]
