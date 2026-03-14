"""Memory programming tool — LLM-driven memory manipulation + explain.

Exposes MemoryProgrammer to the edge chat loop so the LLM can
inject/correct/purge/tune memories and explain execution results.
"""

import json
import logging
import sys
from typing import Any

from cli.tools.base import EdgeTool, SideEffect

logger = logging.getLogger(__name__)

_orig_sh_init = logging.StreamHandler.__init__
def _sh_init_stderr(self: logging.StreamHandler, stream: Any = None) -> None:
    if stream is sys.stdout:
        stream = sys.stderr
    _orig_sh_init(self, stream)
logging.StreamHandler.__init__ = _sh_init_stderr  # type: ignore[method-assign]


class MemoryProgramTool(EdgeTool):
    """Execute memory programs (inject/correct/purge/tune) with explain."""

    def __init__(self, session_info: dict[str, Any] | None = None) -> None:
        self._session = session_info or {}

    @property
    def name(self) -> str:
        return "memory_program"

    @property
    def description(self) -> str:
        return (
            "Write-only tool: inject, correct, purge, or tune user memories. "
            "Use ONLY when user explicitly asks to remember/forget/fix/update a preference. "
            "Do NOT call this to answer questions or recall what was said — use conversation history instead. "
            "Actions: inject (add NEW memory), correct (UPDATE existing memory — use when user changes their mind or corrects a previous statement), "
            "purge (remove by filter), tune (change retrieval strategy). "
            "user_id is optional — defaults to current user."
        )

    @property
    def parameters(self) -> dict[str, Any]:
        return {
            "type": "object",
            "properties": {
                "actions": {
                    "type": "array",
                    "description": (
                        "List of action objects. Each has exactly one key: "
                        "inject ({content, type?, trust?}) — add a NEW memory; "
                        "correct ({memory_id, new_content, reason?}) — update a specific memory by ID (use only if you have the memory_id); "
                        "purge ({filter: {memory_ids?, type?}, reason?}) — deactivate memories; "
                        "tune ({strategy, params?}) — change retrieval strategy. "
                        "To UPDATE a preference (user changes their mind): use purge with type='profile' to remove old, then inject the new one. "
                        "type must be one of: profile, semantic, procedural. "
                        "trust must be one of: T1, T2 (default), T3, T4."
                    ),
                    "items": {"type": "object"},
                },
                "dry_run": {
                    "type": "boolean",
                    "description": "If true, validate only without executing.",
                },
                "explain": {
                    "type": "boolean",
                    "description": "If true, return detailed per-action execution breakdown.",
                },
            },
            "required": ["actions"],
        }

    @property
    def side_effect(self) -> SideEffect:
        return SideEffect.WRITE

    async def execute(self, **kwargs: Any) -> str:
        user_id: str = kwargs.get("user_id") or self._session.get("user_id") or self._session.get("agent_id", "")
        logger.debug("memory_program.execute: user_id=%s session=%s", user_id, self._session)
        if not user_id:
            return json.dumps({"error": "user_id not available in session context"})
        actions: list[dict] = kwargs["actions"]
        dry_run: bool = kwargs.get("dry_run", False)
        explain: bool = kwargs.get("explain", False)

        try:
            programmer = _get_programmer(user_id)
        except Exception as e:
            return json.dumps({"error": f"Failed to initialize: {e}"})

        try:
            result = programmer.execute(
                user_id,
                actions,
                dry_run=dry_run,
                program_name="cli",
                session_id=self._session.get("session_id"),
            )
        except Exception as e:
            return json.dumps({"error": str(e)})

        out: dict[str, Any] = {
            "actions_executed": result.actions_executed,
            "actions_failed": result.actions_failed,
        }
        if result.dry_run:
            out["dry_run"] = True
        if result.timed_out:
            out["timed_out"] = True

        if explain:
            out["explain"] = [
                {
                    "action": r.action_type,
                    "success": r.success,
                    "detail": r.detail,
                    **({"error": r.error} if r.error else {}),
                }
                for r in result.results
            ]
        else:
            out["results"] = [
                {
                    "action": r.action_type,
                    "success": r.success,
                    **({"error": r.error} if r.error else {}),
                }
                for r in result.results
            ]

        return json.dumps(out, default=str)


def _get_programmer(user_id: str | None = None):
    """Create MemoryProgrammer with user-aware editor."""
    from api.database import SessionLocal
    from core.memory.factory import create_editor
    from core.memory.programmer import MemoryProgrammer

    db_factory = SessionLocal
    editor = create_editor(db_factory, user_id=user_id)
    return MemoryProgrammer(editor, db_factory)
