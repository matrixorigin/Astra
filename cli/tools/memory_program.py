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

# Patch StreamHandler so any future handler created with stream=stdout
# (e.g. matrixone client's create_default_logger) goes to stderr instead,
# keeping CLI subprocess stdout clean for response capture.
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
        import os
        # sandbox=False by default.
        #
        # Rationale: sandbox=True requires an explicit commit step to merge the
        # experiment branch into production. Until a review UI exists in the CLI
        # or web frontend, the LLM has no way to trigger that commit — so data
        # written in sandbox mode would silently disappear and memories would
        # never take effect.
        #
        # When a proper review/commit flow is implemented, flip this default to
        # True and update the tool description to explain the two-step workflow.
        #
        # Override via env: MEMORY_PROGRAM_SANDBOX=true (e.g. for manual testing)
        self._default_sandbox = os.environ.get("MEMORY_PROGRAM_SANDBOX", "false").lower() != "false"

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
                "user_id": {
                    "type": "string",
                    "description": "Target user ID. If omitted, defaults to the current user.",
                },
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
                "commit": {
                    "type": "string",
                    "description": "Experiment ID to commit (from a previous sandbox run).",
                },
            },
            "required": ["actions"],
        }

    @property
    def side_effect(self) -> SideEffect:
        return SideEffect.WRITE

    async def execute(self, **kwargs: Any) -> str:
        user_id: str = kwargs.get("user_id") or self._session.get("user_id") or self._session.get("agent_id", "")
        if not user_id:
            return json.dumps({"error": "user_id not available in session context"})
        actions: list[dict] = kwargs["actions"]
        sandbox: bool = kwargs.get("sandbox", self._default_sandbox)
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
                session_id=self._session.get("session_id"),
            )
        except Exception as e:
            return json.dumps({"error": str(e)})

        out: dict[str, Any] = {
            "actions_executed": result.actions_executed,
            "actions_failed": result.actions_failed,
        }
        if result.experiment_id:
            out["experiment_id"] = result.experiment_id
            if sandbox and not dry_run and not result.rolled_back:
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
                {
                    "action": r.action_type,
                    "success": r.success,
                    **({"error": r.error} if r.error else {}),
                }
                for r in result.results
            ]

        return json.dumps(out, default=str)

    async def _commit(self, programmer: Any, experiment_id: str) -> str:
        try:
            programmer._experiments.commit(experiment_id)
            return json.dumps({"committed": experiment_id, "success": True})
        except Exception as e:
            return json.dumps({"error": f"Commit failed: {e}"})


_programmer_instance = None

def _get_programmer():
    """Lazy-init MemoryProgrammer from production DB (singleton).
    
    Skips embedding client init — CLI doesn't need embeddings (server handles that).
    """
    global _programmer_instance
    if _programmer_instance is not None:
        return _programmer_instance
    from api.database import SessionLocal
    from core.memory.experiment import MemoryExperimentManager
    from core.memory.editor import MemoryEditor
    from core.memory.canonical_storage import CanonicalStorage
    from core.memory.programmer import MemoryProgrammer
    from core.memory.factory import _register_builtins

    _register_builtins()
    db_factory = SessionLocal
    storage = CanonicalStorage(db_factory)
    editor = MemoryEditor(storage, db_factory, index_manager=None, embed_client=None)
    db_name = SessionLocal.kw["bind"].url.database
    experiments = MemoryExperimentManager(db_factory, source_db=db_name)
    _programmer_instance = MemoryProgrammer(editor, experiments, db_factory)
    return _programmer_instance
