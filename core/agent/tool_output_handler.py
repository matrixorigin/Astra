"""Tool output handler with mo-memoria integration.

Handles large tool outputs by:
1. Storing full output in mo-memoria (TOOL_RESULT type)
2. Generating structured summary (rule-based, zero LLM cost)
3. Returning summary + memory reference
4. Reusing similar historical results via Retriever
5. Dynamic threshold based on remaining context budget
"""

from __future__ import annotations

from collections.abc import Callable
from typing import TYPE_CHECKING, Any

from core.memory.types import MemoryType

if TYPE_CHECKING:
    pass

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
    """Summarize grep output: per-file stats + samples with line numbers."""
    lines = output.strip().split('\n')

    # Group by file with line numbers
    file_matches: dict[str, list[tuple[int, str]]] = {}
    for line in lines[:1000]:
        if ':' in line:
            parts = line.split(':', 2)
            if len(parts) >= 3:
                file, lineno, content = parts[0], parts[1], parts[2]
                if file not in file_matches:
                    file_matches[file] = []
                try:
                    file_matches[file].append((int(lineno), content.strip()[:80]))
                except ValueError:
                    pass

    # Sort files by match count (descending)
    sorted_files = sorted(file_matches.items(), key=lambda x: -len(x[1]))

    # Build summary with per-file breakdown
    summary_parts = [f"Found {len(lines)} matches in {len(file_matches)} files."]
    summary_parts.append("\nPer-file breakdown:")

    for file, matches in sorted_files[:10]:
        line_nums = [m[0] for m in matches[:5]]
        summary_parts.append(f"  {file}: {len(matches)} matches (lines: {line_nums})")
        # Show 1-2 samples per file with line numbers
        for lineno, content in matches[:2]:
            summary_parts.append(f"    L{lineno}: {content}")

    if len(sorted_files) > 10:
        summary_parts.append(f"  ... and {len(sorted_files) - 10} more files")

    return '\n'.join(summary_parts)


def _summarize_shell(output: str) -> str:
    """Summarize shell output: head + tail + stats."""
    lines = output.strip().split('\n')
    if len(lines) <= 20:
        return output

    return (
        f"Output: {len(lines)} lines, {len(output)} bytes\n"
        f"First 10:\n" + '\n'.join(lines[:10]) + "\n...\n"
        "Last 5:\n" + '\n'.join(lines[-5:])
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

# Tools that should NOT be summarized (need full content for LLM)
NON_SUMMARIZABLE_TOOLS: set[str] = {
    "fs_read",      # Code files need full structure
    "read_file",    # Same
    "cat",          # Same
    "base64",       # Binary content
}


def is_summarizable(tool_name: str, output: str) -> bool:
    """Check if tool output can be safely summarized.
    
    Args:
        tool_name: Name of the tool
        output: Tool output content
    
    Returns:
        True if output can be summarized, False if full content needed
    """
    # Explicit non-summarizable tools
    if tool_name in NON_SUMMARIZABLE_TOOLS:
        return False

    # Heuristics for code content (need full structure)
    code_indicators = ["def ", "class ", "import ", "function ", "const ", "let ", "var "]
    if any(ind in output[:2000] for ind in code_indicators):
        # Looks like code - check if it's a single file read
        lines = output.split('\n')
        if len(lines) < 200:  # Small code file, keep full
            return False

    return True


def register_summary_strategy(tool_name: str, strategy: Callable[[str], str]) -> None:
    """Register a custom summary strategy for a tool.
    
    Args:
        tool_name: Name of the tool
        strategy: Function that takes output string and returns summary
    """
    SUMMARY_GENERATORS[tool_name] = strategy


def _summarize_list_dir(output: str) -> str:
    """Summarize directory listing: top-level dirs with file counts."""
    stripped = output.strip()
    lines = stripped.split('\n') if stripped else []
    top_dirs = []
    top_files = []
    for line in lines:
        stripped = line.strip()
        if not stripped:
            continue
        # Top-level dir: first segment contains "/" but no deeper nesting
        # e.g. "api/", "api/  (42 files)", but not "api/models/foo.py"
        first_slash = stripped.find('/')
        if first_slash >= 0:
            after_slash = stripped[first_slash + 1:].lstrip()
            # Top-level if nothing after slash, or only "(N files)" annotation
            if not after_slash or after_slash.startswith('('):
                top_dirs.append(stripped)
        else:
            top_files.append(stripped)
    return (
        f"Directory listing: {len(lines)} entries\n"
        f"Top-level directories:\n" + '\n'.join(top_dirs[:30]) +
        (f"\n... and {len(top_dirs)-30} more dirs" if len(top_dirs) > 30 else "") +
        (f"\n{len(top_files)} files in root" if top_files else "")
    )


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
        "Last 10 lines:\n" + '\n'.join(lines[-10:])
    )


# Register additional strategies
SUMMARY_GENERATORS.update({
    "fs_read": _summarize_file_content,
    "read_file": _summarize_file_content,
    "web_fetch": _summarize_default,  # HTML too varied for rules
    "api_call": _summarize_json,
    "list_dir": _summarize_list_dir,
    "list_directory": _summarize_list_dir,
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
    memory_service: Any | None,
    turn_event_id: str | None = None,
    remaining_tokens: int | None = None,
    force_full: bool = False,  # Force return full content (no summarization)
) -> str:
    """Process tool output: small returns directly, large stores + summarizes.
    
    Args:
        output: Raw tool output
        tool_name: Name of the tool (grep, shell, etc.)
        session_id: Current session ID
        user_id: Current user ID
        memory_service: Any instance (None = truncation-only fallback)
        turn_event_id: Optional event ID for provenance tracking
        remaining_tokens: Optional remaining context budget for dynamic threshold
        force_full: Force return full content (skip summarization check)
    
    Returns:
        Original output (if small) or summary + memory reference (if large)
    """
    from core.agent.tool_context_metrics import record_tool_output

    threshold = compute_dynamic_threshold(remaining_tokens)

    # Check if should skip summarization
    skip_summary = force_full or not is_summarizable(tool_name, output)

    if len(output) <= threshold or (skip_summary and len(output) <= threshold * 3):
        record_tool_output(tool_name, len(output), len(output), was_summarized=False)
        return output

    # No memory service — fall back to rule-based summary + truncation
    if memory_service is None:
        summary = generate_structured_summary(output, tool_name)
        result = f"{summary}\n\n[Full output ({len(output)} bytes) — truncated, no memory store]"
        record_tool_output(tool_name, len(output), len(result), was_summarized=True)
        return result

    # 1. Store full output in mo-memoria
    import uuid

    from core.memory.types import Memory
    source_events = [turn_event_id] if turn_event_id else []
    try:
        mem_obj = Memory(
            memory_id=uuid.uuid4().hex,
            user_id=user_id,
            memory_type=MemoryType.TOOL_RESULT,
            content=output,
            session_id=session_id,
            source_event_ids=source_events,
        )
        memory = memory_service.create_memory(mem_obj)
    except Exception as e:
        # Fallback: truncate if mo-memoria write fails
        record_tool_output(tool_name, len(output), threshold, was_summarized=True)
        return output[:threshold] + f"\n... [truncated, mo-memoria unavailable: {e}]"

    # 2. Generate rule-based summary
    summary = generate_structured_summary(output, tool_name)

    # 3. Store summary text for replay determinism (摘要也需要持久化)
    result = f"{summary}\n\n[Full output ({len(output)} bytes): memory:{memory.memory_id}]"

    # 4. Record metrics
    record_tool_output(tool_name, len(output), len(result), was_summarized=True)

    return result


def find_similar_result(
    tool_name: str,
    params: dict,
    session_id: str,
    user_id: str,
    memory_service: Any,
    cross_session: bool = False,
    max_age_seconds: int = 300,  # 5 minutes default
) -> str | None:
    """Find similar historical tool result via mo-memoria Retriever.
    
    Args:
        tool_name: Name of the tool
        params: Tool parameters (pattern, path, etc.)
        session_id: Current session ID
        user_id: Current user ID
        memory_service: Any instance
        cross_session: If True, search across all sessions
        max_age_seconds: Maximum age of result to consider (staleness check)
    
    Returns:
        Memory reference if similar result found, None otherwise
    """
    from datetime import datetime, timedelta, timezone

    # Build query from tool name + key params
    query_parts = [tool_name]
    for key in ("pattern", "path", "command", "query"):
        if key in params:
            query_parts.append(str(params[key]))
    query = ' '.join(query_parts)

    results, _ = memory_service.retrieve(
        user_id=user_id,
        query=query,
        session_id=session_id if not cross_session else "global",
        memory_types=[MemoryType.TOOL_RESULT],
        top_k=1,
    )

    if not results:
        return None

    result = results[0]

    # Staleness check: reject if too old.
    # ref_time is UTC-aware (set via _utcnow()), so we must compare with UTC.
    ref_time = result.observed_at or result.created_at
    if ref_time:
        age = datetime.now(timezone.utc) - ref_time
        if age > timedelta(seconds=max_age_seconds):
            return None

    # Check key param match (e.g., same grep pattern)
    if "pattern" in params:
        if params["pattern"] not in result.content[:1000]:
            return None

    return f"[Reusing previous {tool_name} result: memory:{result.memory_id}]"


# --- Memory Expand Tool (for LLM to expand [memory:xxx] references) ---

def expand_memory_reference(
    memory_id: str,
    memory_service: Any,
    start_line: int | None = None,
    end_line: int | None = None,
    query: str | None = None,
    max_chars: int = 10000,  # Prevent re-explosion
) -> str:
    """Expand a memory reference, optionally with range or query filter.
    
    Args:
        memory_id: The memory ID to expand (from [memory:xxx] reference)
        memory_service: Any instance
        start_line: Optional start line for partial expansion
        end_line: Optional end line for partial expansion
        query: Optional query to filter content (grep-like)
        max_chars: Maximum characters to return (prevents context re-explosion)
    
    Returns:
        Expanded content (full or filtered), truncated if exceeds max_chars
    """
    memory = memory_service.get_memory(memory_id)
    if not memory:
        return f"Error: Memory {memory_id} not found"

    content = memory.content
    lines = content.split('\n')
    total_lines = len(lines)

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
            content = f"Filtered {len(matching)} of {total_lines} lines matching '{query}':\n" + '\n'.join(matching[:100])
        else:
            content = f"No lines matching '{query}' in {total_lines} lines"

    # Truncate to prevent context re-explosion
    if len(content) > max_chars:
        content = content[:max_chars] + f"\n... [truncated, use start_line/end_line for pagination, total {len(memory.content)} chars]"

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
