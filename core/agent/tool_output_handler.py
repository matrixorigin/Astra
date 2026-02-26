"""Tool output handler with mo-trustmem integration.

Handles large tool outputs by:
1. Storing full output in mo-trustmem (TOOL_RESULT type)
2. Generating structured summary (rule-based, zero LLM cost)
3. Returning summary + memory reference
4. Reusing similar historical results via Retriever
5. Dynamic threshold based on remaining context budget
"""

from __future__ import annotations

import json
from typing import TYPE_CHECKING, Callable

from core.memory.types import MemoryType

if TYPE_CHECKING:
    from core.memory.retriever import MemoryRetriever
    from core.memory.store import MemoryStore

SUMMARY_THRESHOLD = 10 * 1024  # 10KB default
MIN_THRESHOLD = 2 * 1024      # 2KB minimum (always summarize if larger)
MAX_THRESHOLD = 50 * 1024     # 50KB maximum (never skip summarization above this)


def compute_dynamic_threshold(remaining_tokens: int | None) -> int:
    """Compute summary threshold based on remaining context budget.
    
    Args:
        remaining_tokens: Estimated remaining tokens in context window.
                         None means use default threshold.
    
    Returns:
        Threshold in bytes. Outputs larger than this will be summarized.
    """
    if remaining_tokens is None:
        return SUMMARY_THRESHOLD
    
    # Heuristic: allow ~20% of remaining budget for tool output
    # 1 token ≈ 4 chars
    budget_bytes = int(remaining_tokens * 4 * 0.2)
    
    # Clamp to reasonable range
    return max(MIN_THRESHOLD, min(MAX_THRESHOLD, budget_bytes))


# --- Structured Summary Generators (rule-based, zero LLM cost) ---

def _summarize_grep(output: str) -> str:
    """Summarize grep output: file stats + sample matches."""
    lines = output.strip().split('\n')
    files: dict[str, int] = {}
    for line in lines[:500]:
        if ':' in line:
            f = line.split(':')[0]
            files[f] = files.get(f, 0) + 1
    
    top_files = sorted(files.items(), key=lambda x: -x[1])[:10]
    file_list = ', '.join(f"{f}({n})" for f, n in top_files)
    if len(files) > 10:
        file_list += f"... (+{len(files) - 10} more)"
    
    return (
        f"Found {len(lines)} matches in {len(files)} files.\n"
        f"Top files: {file_list}\n"
        f"Sample:\n" + '\n'.join(lines[:5])
    )


def _summarize_shell(output: str) -> str:
    """Summarize shell output: head + tail + stats."""
    lines = output.strip().split('\n')
    if len(lines) <= 20:
        return output
    
    return (
        f"Output: {len(lines)} lines, {len(output)} bytes\n"
        f"First 10:\n" + '\n'.join(lines[:10]) + "\n...\n"
        f"Last 5:\n" + '\n'.join(lines[-5:])
    )


def _summarize_default(output: str) -> str:
    """Default: truncate with stats."""
    return output[:2000] + f"\n... ({len(output)} bytes total)"


SUMMARY_GENERATORS: dict[str, Callable[[str], str]] = {
    "grep": _summarize_grep,
    "shell": _summarize_shell,
    "execute_bash": _summarize_shell,
    "git": _summarize_shell,
}


def register_summary_strategy(tool_name: str, strategy: Callable[[str], str]) -> None:
    """Register a custom summary strategy for a tool.
    
    Args:
        tool_name: Name of the tool
        strategy: Function that takes output string and returns summary
    """
    SUMMARY_GENERATORS[tool_name] = strategy


def _summarize_json(output: str) -> str:
    """Summarize JSON output: keys + sample values."""
    try:
        import json
        data = json.loads(output)
        if isinstance(data, dict):
            keys = list(data.keys())[:20]
            return f"JSON object with {len(data)} keys: {', '.join(keys)}{'...' if len(data) > 20 else ''}"
        elif isinstance(data, list):
            return f"JSON array with {len(data)} items. First item keys: {list(data[0].keys()) if data and isinstance(data[0], dict) else 'N/A'}"
    except Exception:
        pass
    return _summarize_default(output)


def _summarize_file_content(output: str) -> str:
    """Summarize file content: line count + head + tail."""
    lines = output.strip().split('\n')
    if len(lines) <= 30:
        return output
    return (
        f"File content: {len(lines)} lines, {len(output)} bytes\n"
        f"First 15 lines:\n" + '\n'.join(lines[:15]) + "\n...\n"
        f"Last 10 lines:\n" + '\n'.join(lines[-10:])
    )


# Register additional strategies
SUMMARY_GENERATORS.update({
    "fs_read": _summarize_file_content,
    "read_file": _summarize_file_content,
    "web_fetch": _summarize_default,  # HTML too varied for rules
    "api_call": _summarize_json,
})


def generate_structured_summary(output: str, tool_name: str) -> str:
    """Generate rule-based structured summary (zero LLM cost)."""
    generator = SUMMARY_GENERATORS.get(tool_name, _summarize_default)
    return generator(output)


# --- Main Handler ---

def process_tool_output(
    output: str,
    tool_name: str,
    session_id: str,
    user_id: str,
    memory_store: MemoryStore,
    turn_event_id: str | None = None,
    remaining_tokens: int | None = None,
) -> str:
    """Process tool output: small returns directly, large stores + summarizes.
    
    Args:
        output: Raw tool output
        tool_name: Name of the tool (grep, shell, etc.)
        session_id: Current session ID
        user_id: Current user ID
        memory_store: mo-trustmem MemoryStore instance
        turn_event_id: Optional event ID for provenance tracking
        remaining_tokens: Optional remaining context budget for dynamic threshold
    
    Returns:
        Original output (if small) or summary + memory reference (if large)
    """
    threshold = compute_dynamic_threshold(remaining_tokens)
    if len(output) <= threshold:
        return output
    
    # 1. Store full output in mo-trustmem
    source_events = [turn_event_id] if turn_event_id else []
    memory = memory_store.create(
        user_id=user_id,
        content=output,
        memory_type=MemoryType.TOOL_RESULT,
        session_id=session_id,
        source=f"tool:{tool_name}",
        source_event_ids=source_events,
        metadata={"tool": tool_name, "size": len(output)},
    )
    
    # 2. Generate rule-based summary
    summary = generate_structured_summary(output, tool_name)
    
    # 3. Return summary + reference
    return f"{summary}\n\n[Full output ({len(output)} bytes): memory:{memory.memory_id}]"


def find_similar_result(
    tool_name: str,
    params: dict,
    session_id: str,
    user_id: str,
    retriever: MemoryRetriever,
    cross_session: bool = False,
    max_age_seconds: int = 300,  # 5 minutes default
) -> str | None:
    """Find similar historical tool result via mo-trustmem Retriever.
    
    Args:
        tool_name: Name of the tool
        params: Tool parameters (pattern, path, etc.)
        session_id: Current session ID
        user_id: Current user ID
        retriever: mo-trustmem MemoryRetriever instance
        cross_session: If True, search across all sessions
        max_age_seconds: Maximum age of result to consider (staleness check)
    
    Returns:
        Memory reference if similar result found, None otherwise
    """
    from datetime import datetime, timedelta
    
    # Build query from tool name + key params
    query_parts = [tool_name]
    for key in ("pattern", "path", "command", "query"):
        if key in params:
            query_parts.append(str(params[key]))
    query = ' '.join(query_parts)
    
    results = retriever.retrieve(
        user_id=user_id,
        query=query,
        session_id=None if cross_session else session_id,
        memory_types=[MemoryType.TOOL_RESULT],
        limit=1,
    )
    
    if not results:
        return None
    
    result = results[0]
    
    # Verify same tool
    if result.metadata.get("tool") != tool_name:
        return None
    
    # Staleness check: reject if too old
    if result.created_at:
        age = datetime.now() - result.created_at
        if age > timedelta(seconds=max_age_seconds):
            return None
    
    # Check key param match (e.g., same grep pattern)
    if "pattern" in params:
        old_content = result.content
        if params["pattern"] not in old_content[:1000]:
            return None
    
    return f"[Reusing previous {tool_name} result: memory:{result.memory_id}]"


# --- Memory Expand Tool (for LLM to expand [memory:xxx] references) ---

def expand_memory_reference(
    memory_id: str,
    memory_store: MemoryStore,
    start_line: int | None = None,
    end_line: int | None = None,
    query: str | None = None,
) -> str:
    """Expand a memory reference, optionally with range or query filter.
    
    Args:
        memory_id: The memory ID to expand (from [memory:xxx] reference)
        memory_store: mo-trustmem MemoryStore instance
        start_line: Optional start line for partial expansion
        end_line: Optional end line for partial expansion
        query: Optional query to filter content (grep-like)
    
    Returns:
        Expanded content (full or filtered)
    """
    memory = memory_store.get(memory_id)
    if not memory:
        return f"Error: Memory {memory_id} not found"
    
    content = memory.content
    lines = content.split('\n')
    
    # Apply line range filter
    if start_line is not None or end_line is not None:
        start = (start_line or 1) - 1
        end = end_line or len(lines)
        lines = lines[start:end]
        content = '\n'.join(lines)
    
    # Apply query filter
    if query:
        matching = [l for l in lines if query.lower() in l.lower()]
        if matching:
            content = f"Filtered {len(matching)} lines matching '{query}':\n" + '\n'.join(matching[:50])
        else:
            content = f"No lines matching '{query}'"
    
    return content


# Tool schema for LLM
MEMORY_EXPAND_TOOL_SCHEMA = {
    "type": "function",
    "function": {
        "name": "memory_expand",
        "description": "Expand a [memory:xxx] reference to see full content. Use when you need details from a summarized tool output.",
        "parameters": {
            "type": "object",
            "properties": {
                "memory_id": {
                    "type": "string",
                    "description": "The memory ID from [memory:xxx] reference"
                },
                "start_line": {
                    "type": "integer",
                    "description": "Optional: start line for partial view"
                },
                "end_line": {
                    "type": "integer",
                    "description": "Optional: end line for partial view"
                },
                "query": {
                    "type": "string",
                    "description": "Optional: filter lines containing this text"
                },
            },
            "required": ["memory_id"],
        },
    },
}
