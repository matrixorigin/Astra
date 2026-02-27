"""Pure functions for OpenAI message history manipulation.

No database or framework dependencies — safe to import in unit tests
without triggering DB connections.
"""

from __future__ import annotations

import json
from typing import Any


def merge_tool_results_into_history(
    history: list[dict[str, Any]],
    tool_results: list[dict[str, Any]] | None,
) -> set[str]:
    """Merge incoming edge tool_results into the correct position in history,
    then heal any remaining orphaned tool_calls with placeholders.

    This two-phase approach handles all edge-cloud failure combinations:

    Phase 1 — MERGE: For each incoming tool_result, find the assistant message
    that requested it (by tool_call_id) and insert the tool message in the
    correct position (right after the assistant's tool_call block).  This
    handles: cloud restart (edge still has results), partial results (edge
    sends some but not all), and normal operation.

    Phase 2 — HEAL: Scan for any assistant tool_calls that STILL lack matching
    tool messages after merging.  These are truly abandoned (edge disconnected,
    max-turns, crash).  Insert placeholders so the LLM API never rejects.

    Returns consumed tool_call_ids (so caller knows which were merged).

    Failure scenarios covered:
      1. Edge disconnects, never sends tool_results → Phase 2 heals all
      2. Edge sends partial tool_results → Phase 1 merges available, Phase 2 heals rest
      3. Cloud restarts, edge sends tool_results normally → Phase 1 merges into correct position
      4. Cloud restarts, edge sends partial tool_results → Phase 1 + Phase 2
      5. Cloud restarts, edge already gave up → Phase 2 heals all
      6. DB has trailing tool_calls with no tool_result → Phase 2 heals
      7. tool_results for unknown tool_call_ids → ignored (returned as unconsumed)
    """
    consumed: set[str] = set()

    # ── Phase 1: Merge incoming tool_results into correct history position ──
    if tool_results:
        pending: dict[str, dict[str, Any]] = {}
        for tr in tool_results:
            tc_id = tr.get("tool_call_id", "")
            if tc_id and tc_id not in pending:
                pending[tc_id] = tr

        _PLACEHOLDER_MARKER = "[not executed"
        inserts: list[tuple[int, dict[str, Any]]] = []
        for i, msg in enumerate(history):
            if msg.get("role") != "assistant" or not msg.get("tool_calls"):
                continue
            block_end = i + 1
            for j in range(i + 1, len(history)):
                if history[j].get("role") == "tool":
                    block_end = j + 1
                else:
                    break
            existing: dict[str, tuple[int, bool]] = {}
            for j in range(i + 1, block_end):
                if history[j].get("role") == "tool":
                    tc_id = history[j].get("tool_call_id", "")
                    is_placeholder = _PLACEHOLDER_MARKER in history[j].get("content", "")
                    existing[tc_id] = (j, is_placeholder)
            insert_at = block_end
            for tc in msg["tool_calls"]:
                tc_id = tc["id"]
                if tc_id not in pending:
                    continue
                if tc_id in existing:
                    idx, is_placeholder = existing[tc_id]
                    if is_placeholder:
                        history[idx]["content"] = pending[tc_id].get("result", "")
                    consumed.add(tc_id)
                else:
                    inserts.append((insert_at, {
                        "role": "tool",
                        "tool_call_id": tc_id,
                        "content": pending[tc_id].get("result", ""),
                    }))
                    consumed.add(tc_id)
                    insert_at += 1

        for pos, tool_msg in reversed(inserts):
            history.insert(pos, tool_msg)

    # ── Phase 2: Heal any remaining orphaned tool_calls with placeholders ──
    inserts_heal: list[tuple[int, dict[str, Any]]] = []
    for i, msg in enumerate(history):
        if msg.get("role") != "assistant" or not msg.get("tool_calls"):
            continue
        expected = {tc["id"] for tc in msg["tool_calls"]}
        found: set[str] = set()
        for j in range(i + 1, len(history)):
            if history[j].get("role") == "tool":
                found.add(history[j].get("tool_call_id", ""))
            else:
                break
        missing = expected - found
        if missing:
            insert_at = i + 1 + len(found)
            for tc in msg["tool_calls"]:
                if tc["id"] in missing:
                    inserts_heal.append((insert_at, {
                        "role": "tool",
                        "tool_call_id": tc["id"],
                        "content": "[not executed -- edge disconnected]",
                    }))
                    insert_at += 1
    for pos, placeholder in reversed(inserts_heal):
        history.insert(pos, placeholder)

    return consumed


def append_recovered_events(
    history: list[dict[str, Any]], rows: list,
) -> list[dict[str, Any]]:
    """Append DB event rows to an existing history list (OpenAI message format).

    Used by both snapshot post-fill and full event-by-event reconstruction.
    Handles tool_call batching: accumulates tool_call events, flushes them as
    one assistant message when the first tool_result arrives.
    """
    pending_tool_calls: list[dict[str, Any]] = []
    in_tool_batch = False

    for row in rows:
        etype, content = row[0], row[1] or ""
        meta = row[2] if len(row) > 2 else None
        if isinstance(meta, str):
            try:
                meta = json.loads(meta)
            except (json.JSONDecodeError, TypeError):
                meta = {}
        meta = meta or {}

        if etype == "user_query":
            in_tool_batch = False
            history.append({"role": "user", "content": content})
        elif etype == "tool_call":
            try:
                tc_data = json.loads(content) if isinstance(content, str) else {}
            except (json.JSONDecodeError, TypeError):
                tc_data = {}
            pending_tool_calls.append({
                "id": tc_data.get("tool_call_id", meta.get("tool_call_id", "")),
                "type": "function",
                "function": {
                    "name": tc_data.get("name", meta.get("name", "")),
                    "arguments": tc_data.get("arguments", "{}"),
                },
            })
        elif etype == "tool_result":
            tool_call_id = meta.get("tool_call_id", "")
            tool_name = meta.get("name", "")
            if pending_tool_calls:
                history.append({"role": "assistant", "content": "", "tool_calls": pending_tool_calls})
                pending_tool_calls = []
                in_tool_batch = True
            elif not in_tool_batch:
                if not tool_call_id:
                    continue
                history.append({"role": "assistant", "content": "", "tool_calls": [{
                    "id": tool_call_id, "type": "function",
                    "function": {"name": tool_name, "arguments": "{}"},
                }]})
                in_tool_batch = True
            try:
                result_data = json.loads(content) if isinstance(content, str) else {}
            except (json.JSONDecodeError, TypeError):
                result_data = {}
            history.append({
                "role": "tool",
                "tool_call_id": tool_call_id,
                "content": result_data.get("result", content)[:4000] if isinstance(result_data, dict) else str(content)[:4000],
            })
        elif etype == "llm_response":
            in_tool_batch = False
            if pending_tool_calls:
                history.append({"role": "assistant", "content": "", "tool_calls": pending_tool_calls})
                pending_tool_calls = []
            history.append({"role": "assistant", "content": content})

    # Flush any trailing tool_calls that had no tool_result in DB
    # (e.g. API crashed mid-execution). merge_tool_results_into_history will
    # add placeholder tool messages later.
    if pending_tool_calls:
        history.append({"role": "assistant", "content": "", "tool_calls": pending_tool_calls})

    return history
