"""Memory programming tool — LLM-driven memory manipulation + explain.

Exposes MemoryProgrammer to the edge chat loop so the LLM can
inject/correct/purge/tune memories and explain execution results.
"""

import json
import logging
from typing import Any

from cli.tools.base import EdgeTool, SideEffect

logger = logging.getLogger(__name__)


class MemoryProgramTool(EdgeTool):
    """Execute memory programs (inject/correct/purge/tune) with explain."""

    @property
    def name(self) -> str:
        return "memory_program"

    @property
    def description(self) -> str:
        return (
            "Execute a memory program to inject, correct, purge, or tune user memories. "
            "Use when user says: remember this, forget that, fix a memory, "
            "update what you know, change memory strategy, or bulk memory operations. "
            "Supports sandbox mode (safe preview before commit), dry-run, "
            "and explain mode that shows per-action execution details. "
            "Actions: inject (add memory), correct (fix existing), "
            "purge (remove by filter), tune (change retrieval strategy)."
        )

    @property
    def parameters(self) -> dict[str, Any]:
        return {
            "type": "object",
            "properties": {
                "user_id": {
                    "type": "string",
                    "description": "Target user ID.",
                },
                "actions": {
                    "type": "array",
                    "description": (
                        "List of action objects. Each has exactly one key: "
                        "inject ({content, type?, trust?}), "
                        "correct ({memory_id, new_content, reason?}), "
                        "purge ({filter: {memory_ids?, type?}, reason?}), "
                        "tune ({strategy, params?})."
                    ),
                    "items": {"type": "object"},
                },
                "sandbox": {
                    "type": "boolean",
                    "description": "If true, execute in isolated branch (default: true). Use false for direct writes.",
                },
                "dry_run": {
                    "type": "boolean",
                    "description": "If true, validate only without executing.",
                },
                "explain": {
                    "type": "boolean",
                    "description": "If true, return detailed per-action execution breakdown.",
                },
                "commit": {
                    "type": "string",
                    "description": "Experiment ID to commit (from a previous sandbox run).",
                },
            },
            "required": ["user_id", "actions"],
        }

    @property
    def side_effect(self) -> SideEffect:
        return SideEffect.WRITE

    async def execute(self, **kwargs: Any) -> str:
        user_id: str = kwargs["user_id"]
        actions: list[dict] = kwargs["actions"]
        sandbox: bool = kwargs.get("sandbox", True)
        dry_run: bool = kwargs.get("dry_run", False)
        explain: bool = kwargs.get("explain", False)
        commit_id: str | None = kwargs.get("commit")

        try:
            programmer = _get_programmer()
        except Exception as e:
            return json.dumps({"error": f"Failed to initialize: {e}"})

        # Commit a previous sandbox experiment
        if commit_id:
            return await self._commit(programmer, commit_id)

        try:
            result = programmer.execute(
                user_id,
                actions,
                sandbox=sandbox,
                dry_run=dry_run,
                program_name="cli",
            )
        except Exception as e:
            return json.dumps({"error": str(e)})

        out: dict[str, Any] = {
            "actions_executed": result.actions_executed,
            "actions_failed": result.actions_failed,
        }
        if result.experiment_id:
            out["experiment_id"] = result.experiment_id
            if sandbox and not dry_run:
                out["hint"] = "Sandbox run. Call with commit=experiment_id to apply."
        if result.dry_run:
            out["dry_run"] = True
        if result.rolled_back:
            out["rolled_back"] = True
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
            # Summary only
            out["results"] = [
                {"action": r.action_type, "success": r.success}
                for r in result.results
            ]

        return json.dumps(out, default=str)

    async def _commit(self, programmer: Any, experiment_id: str) -> str:
        try:
            programmer._experiments.commit(experiment_id)
            return json.dumps({"committed": experiment_id, "success": True})
        except Exception as e:
            return json.dumps({"error": f"Commit failed: {e}"})


def _get_programmer():
    """Lazy-init MemoryProgrammer from production DB."""
    from api.database import SessionLocal
    from core.memory.canonical_storage import CanonicalStorage
    from core.memory.editor import MemoryEditor
    from core.memory.experiment import MemoryExperimentManager
    from core.memory.programmer import MemoryProgrammer

    db_factory = SessionLocal
    storage = CanonicalStorage(db_factory)
    editor = MemoryEditor(storage, db_factory)
    db_name = SessionLocal.kw["bind"].url.database
    experiments = MemoryExperimentManager(db_factory, source_db=db_name)
    return MemoryProgrammer(editor, experiments, db_factory)
