"""Tool output handler with mo-trustmem integration.

Handles large tool outputs by:
1. Storing full output in mo-trustmem (TOOL_RESULT type)
2. Generating structured summary (rule-based, zero LLM cost)
3. Returning summary + memory reference
4. Reusing similar historical results via Retriever
"""

from __future__ import annotations

import json
from typing import TYPE_CHECKING, Callable

from core.memory.types import MemoryType

if TYPE_CHECKING:
    from core.memory.retriever import MemoryRetriever
    from core.memory.store import MemoryStore

SUMMARY_THRESHOLD = 10 * 1024  # 10KB


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
) -> str:
    """Process tool output: small returns directly, large stores + summarizes.
    
    Args:
        output: Raw tool output
        tool_name: Name of the tool (grep, shell, etc.)
        session_id: Current session ID
        user_id: Current user ID
        memory_store: mo-trustmem MemoryStore instance
        turn_event_id: Optional event ID for provenance tracking
    
    Returns:
        Original output (if small) or summary + memory reference (if large)
    """
    if len(output) <= SUMMARY_THRESHOLD:
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
) -> str | None:
    """Find similar historical tool result via mo-trustmem Retriever.
    
    Args:
        tool_name: Name of the tool
        params: Tool parameters (pattern, path, etc.)
        session_id: Current session ID
        user_id: Current user ID
        retriever: mo-trustmem MemoryRetriever instance
        cross_session: If True, search across all sessions
    
    Returns:
        Memory reference if similar result found, None otherwise
    """
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
    # Verify same tool and similar params
    if result.metadata.get("tool") != tool_name:
        return None
    
    # Check key param match (e.g., same grep pattern)
    if "pattern" in params:
        old_content = result.content
        if params["pattern"] not in old_content[:1000]:
            return None
    
    return f"[Reusing previous {tool_name} result: memory:{result.memory_id}]"
