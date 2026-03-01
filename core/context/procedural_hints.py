"""Procedural memory conflict resolution.

P0 Critical: Zero-cost helpers to detect when user instructions contradict
procedural memory patterns, ensuring user intent always wins.

Design ref: context-window-management.md §1 Priority Resolution Mechanism
"""

from __future__ import annotations

import re


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
