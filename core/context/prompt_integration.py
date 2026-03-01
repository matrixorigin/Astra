"""Integration helper for prompt assembly with reference-aware compression.

Bridges prompt_assembler.py with history_compression.py and reference_tracking.py.
"""

from __future__ import annotations

from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from typing import Any


def integrate_compression_into_prompt(
    history: list[dict[str, Any]],
    current_turn_response: str,
    current_turn_tool_calls: list[dict[str, Any]],
    elastic_budget: int,
    enable_compression: bool = True
) -> str:
    """
    Integrate reference-aware compression into prompt assembly.
    
    This is the integration point between PromptAssembler and the new
    compression system. Called from PromptAssembler._build_history().
    
    Args:
        history: Conversation history turns
        current_turn_response: Current LLM response (if any)
        current_turn_tool_calls: Current tool calls (if any)
        elastic_budget: Token budget for history section
        enable_compression: Feature flag
        
    Returns:
        Formatted history string for prompt
    """
    if not enable_compression or len(history) <= 3:
        # No compression needed
        return _format_history_simple(history)
    
    # Import here to avoid circular dependency
    from core.context.reference_tracking import analyze_semantic_references
    from core.context.history_compression import compress_history_with_references
    
    # Analyze which events are referenced
    referenced_events = analyze_semantic_references(
        current_turn_response,
        current_turn_tool_calls,
        history
    )
    
    # Compress with reference preservation
    compressed = compress_history_with_references(
        history,
        referenced_events,
        elastic_budget
    )
    
    # Format for prompt
    return _format_compressed_history(compressed)


def _format_history_simple(history: list[dict[str, Any]]) -> str:
    """Format history without compression."""
    parts = []
    for i, turn in enumerate(history):
        parts.append(f"Turn {i+1}:")
        if "user_query" in turn:
            parts.append(f"User: {turn['user_query']}")
        if "llm_response" in turn:
            parts.append(f"Assistant: {turn['llm_response']}")
    return "\n".join(parts)


def _format_compressed_history(compressed: dict[str, Any]) -> str:
    """Format compressed history for prompt."""
    parts = []
    
    # Tier 3: Synopsis (if exists)
    if compressed.get("tier3"):
        parts.append("=== Session Synopsis ===")
        parts.append(compressed["tier3"])
        parts.append("")
    
    # Tier 2: Middle turns (compressed)
    if compressed.get("tier2"):
        parts.append("=== Earlier Context ===")
        for turn in compressed["tier2"]:
            if "user_query" in turn:
                parts.append(f"User: {turn['user_query']}")
            
            # Tool results (full or summary)
            for result in turn.get("tool_results", []):
                if "summary" in result:
                    parts.append(f"  {result['summary']}")
                elif "content" in result:
                    parts.append(f"  {result['tool_name']}: {result['content'][:200]}...")
            
            if "llm_response" in turn:
                parts.append(f"Assistant: {turn['llm_response']}")
        parts.append("")
    
    # Tier 1: Recent turns (full fidelity)
    if compressed.get("tier1"):
        parts.append("=== Recent Context ===")
        for turn in compressed["tier1"]:
            if "user_query" in turn:
                parts.append(f"User: {turn['user_query']}")
            if "llm_response" in turn:
                parts.append(f"Assistant: {turn['llm_response']}")
    
    return "\n".join(parts)
