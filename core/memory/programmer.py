"""MemoryProgrammer — declarative memory manipulation via structured scripts.

Thin orchestrator over MemoryEditor.
Parses YAML/dict scripts, validates actions, executes directly.
"""

from __future__ import annotations

import logging
from dataclasses import dataclass, field
from typing import TYPE_CHECKING, Any

from pydantic import BaseModel, Field

if TYPE_CHECKING:
    from core.db_consumer import DbFactory
    from core.memory.editor import MemoryEditor


logger = logging.getLogger(__name__)

SCRIPT_VERSION = 1


# ── Action schemas ────────────────────────────────────────────────────


class InjectAction(BaseModel):
    """Inject a new memory."""

    inject: dict = Field(...)

    @property
    def content(self) -> str:
        return self.inject["content"]

    @property
    def memory_type(self) -> str:
        return self.inject.get("type", "semantic")

    @property
    def trust_tier(self) -> str:
        return self.inject.get("trust", "T2")


class CorrectAction(BaseModel):
    """Correct an existing memory."""

    correct: dict = Field(...)

    @property
    def memory_id(self) -> str:
        return self.correct["memory_id"]

    @property
    def new_content(self) -> str:
        return self.correct["new_content"]


class PurgeAction(BaseModel):
    """Purge memories matching filter."""

    purge: dict = Field(...)

    @property
    def filter_spec(self) -> dict:
        return self.purge.get("filter", {})


class TuneAction(BaseModel):
    """Tune strategy params."""

    tune: dict = Field(...)

    @property
    def strategy(self) -> str:
        return self.tune["strategy"]

    @property
    def params(self) -> dict:
        return self.tune.get("params", {})


_ACTION_KEYS = {"inject", "correct", "purge", "tune"}


class InvalidScriptError(ValueError):
    """Raised when a memory program script is invalid."""


class ProgramTimeoutError(TimeoutError):
    """Raised when a memory program exceeds its timeout."""


@dataclass
class ActionResult:
    """Result of a single action execution."""

    action_type: str
    success: bool
    detail: dict[str, Any] = field(default_factory=dict)
    error: str | None = None


@dataclass
class ProgramResult:
    """Result of executing a memory program."""

    actions_executed: int = 0
    actions_failed: int = 0
    results: list[ActionResult] = field(default_factory=list)
    dry_run: bool = False
    timed_out: bool = False


def parse_script(raw: str | dict | list) -> list[dict]:
    """Parse a memory program script into a list of action dicts."""
    if isinstance(raw, str):
        import re
        import yaml
        raw = re.sub(r"^```(?:ya?ml)?\s*\n", "", raw.strip())
        raw = re.sub(r"\n```\s*$", "", raw)
        try:
            raw = yaml.safe_load(raw)
        except Exception as e:
            raise InvalidScriptError(f"Invalid YAML: {e}") from e

    if isinstance(raw, dict):
        if "actions" in raw:
            version = raw.get("version", 1)
            if int(version) != SCRIPT_VERSION:
                raise InvalidScriptError(
                    f"Unsupported script version {version} (expected {SCRIPT_VERSION})"
                )
            actions = raw["actions"]
        else:
            actions = [raw]
    elif isinstance(raw, list):
        actions = raw
    else:
        raise InvalidScriptError(f"Expected dict, list, or YAML string, got {type(raw).__name__}")

    if not actions:
        raise InvalidScriptError("Script has no actions")

    for i, action in enumerate(actions):
        if not isinstance(action, dict):
            raise InvalidScriptError(f"Action {i} is not a dict")
        if "action" in action and action["action"] in _ACTION_KEYS:
            action_type = action.pop("action")
            actions[i] = {action_type: action}
            action = actions[i]
        keys = set(action.keys()) & _ACTION_KEYS
        if len(keys) == 0:
            raise InvalidScriptError(
                f"Action {i} has no recognized action key (expected one of {_ACTION_KEYS})"
            )
        if len(keys) > 1:
            raise InvalidScriptError(f"Action {i} has multiple action keys: {keys}")

    return [_normalize_action_fields(a) for a in actions]


_FIELD_ALIASES: dict[str, str] = {
    "memory_type": "type",
    "kind": "type",
    "trust_tier": "trust",
    "confidence_level": "trust",
    "tier": "trust",
    "text": "content",
    "message": "content",
    "body": "content",
    "new_text": "new_content",
    "new_message": "new_content",
    "updated_content": "new_content",
    "strategy_key": "strategy",
    "strategy_name": "strategy",
}


def _normalize_action_fields(action: dict) -> dict:
    """Normalize field names inside each action's spec dict."""
    normalized = {}
    for key, spec in action.items():
        if key in _ACTION_KEYS and isinstance(spec, dict):
            normalized[key] = _remap_fields(spec)
        else:
            normalized[key] = spec
    return normalized


def _remap_fields(spec: dict) -> dict:
    """Remap alias field names to canonical names."""
    out: dict = {}
    for k, v in spec.items():
        canonical = _FIELD_ALIASES.get(k, k)
        if canonical not in out:
            out[canonical] = _remap_fields(v) if isinstance(v, dict) else v
    return out


def _get_action_type(action: dict) -> str:
    """Get the action type key from an action dict."""
    for key in _ALL_ACTION_KEYS:
        if key in action:
            return key
    raise InvalidScriptError(f"No action key found in {action}")


_BATCH_INJECT_KEY = "_batch_inject"
_ALL_ACTION_KEYS = _ACTION_KEYS | {_BATCH_INJECT_KEY}


def _coalesce_injects(actions: list[dict]) -> list[dict]:
    """Merge consecutive inject actions into batch inserts."""
    result: list[dict] = []
    batch: list[dict] = []

    def flush() -> None:
        if len(batch) == 1:
            result.append({"inject": batch[0]})
        elif len(batch) > 1:
            result.append({_BATCH_INJECT_KEY: batch[:]})
        batch.clear()

    for action in actions:
        if "inject" in action:
            batch.append(action["inject"])
        else:
            flush()
            result.append(action)
    flush()
    return result


class MemoryProgrammer:
    """Declarative memory manipulation via structured scripts."""

    def __init__(
        self,
        editor: MemoryEditor,
        db_factory: DbFactory,
    ) -> None:
        self._editor = editor
        self._db_factory = db_factory

    def execute(
        self,
        user_id: str,
        script: str | dict | list,
        *,
        dry_run: bool = False,
        atomic: bool = True,
        timeout_seconds: float | None = None,
        program_name: str = "unnamed",
        session_id: str | None = None,
    ) -> ProgramResult:
        """Parse and execute a memory program."""
        import time

        actions = parse_script(script)

        if dry_run:
            return ProgramResult(
                actions_executed=0,
                results=[
                    ActionResult(action_type=_get_action_type(a), success=True,
                                 detail={"dry_run": True})
                    for a in actions
                ],
                dry_run=True,
            )

        actions = _coalesce_injects(actions)

        deadline = (time.monotonic() + timeout_seconds) if timeout_seconds else None
        results: list[ActionResult] = []
        timed_out = False
        for action in actions:
            if atomic and results and not results[-1].success:
                break
            if deadline and time.monotonic() >= deadline:
                timed_out = True
                break
            result = self._execute_action(user_id, action, session_id=session_id)
            results.append(result)

        executed = sum(1 for r in results if r.success)
        failed = sum(1 for r in results if not r.success)

        if timed_out:
            raise ProgramTimeoutError(
                f"Execution timed out after {timeout_seconds}s "
                f"({len(results)}/{len(actions)} actions completed)"
            )

        pr = ProgramResult(
            actions_executed=executed,
            actions_failed=failed,
            results=results,
        )
        self._log_program_audit(user_id, program_name, pr)
        return pr

    def _log_program_audit(
        self, user_id: str, program_name: str, result: ProgramResult,
    ) -> None:
        """Write a program-level entry to mem_edit_log."""
        import json
        from sqlalchemy import text
        from core.utils.id_generator import generate_id

        memory_ids = []
        for r in result.results:
            if r.success and r.detail:
                if "memory_id" in r.detail:
                    memory_ids.append(r.detail["memory_id"])
                elif "memory_ids" in r.detail:
                    memory_ids.extend(r.detail["memory_ids"])
        try:
            with self._db_factory() as db:
                db.execute(
                    text(
                        "INSERT INTO mem_edit_log "
                        "(edit_id, user_id, operation, target_ids, reason, "
                        " snapshot_before, created_by) "
                        "VALUES (:eid, :uid, :op, :tids, :reason, :snap, :uid)"
                    ),
                    {
                        "eid": generate_id(),
                        "uid": user_id,
                        "op": "program",
                        "tids": json.dumps(memory_ids),
                        "reason": program_name,
                        "snap": None,
                    },
                )
                db.commit()
        except Exception:
            logger.debug("Failed to log program audit for %s", user_id, exc_info=True)

    def _execute_action(self, user_id: str, action: dict, *, session_id: str | None = None) -> ActionResult:
        """Execute a single action via MemoryEditor."""
        raw_type = _get_action_type(action)
        display_type = "inject" if raw_type == _BATCH_INJECT_KEY else raw_type
        try:
            if raw_type == "inject":
                return self._do_inject(user_id, action["inject"], session_id=session_id)
            if raw_type == _BATCH_INJECT_KEY:
                return self._do_batch_inject(user_id, action[_BATCH_INJECT_KEY], session_id=session_id)
            if raw_type == "correct":
                return self._do_correct(user_id, action["correct"])
            if raw_type == "purge":
                return self._do_purge(user_id, action["purge"])
            if raw_type == "tune":
                return self._do_tune(user_id, action["tune"])
            return ActionResult(action_type=display_type, success=False,
                                error=f"Unknown action: {display_type}")
        except Exception as e:
            return ActionResult(action_type=display_type, success=False, error=str(e))

    def _do_batch_inject(
        self, user_id: str, specs: list[dict], *, session_id: str | None = None,
    ) -> ActionResult:
        for spec in specs:
            if not spec.get("content"):
                return ActionResult(
                    action_type="inject", success=False,
                    error="inject requires 'content'",
                )

        stored = self._editor.batch_inject(user_id, specs, source="batch_inject", session_id=session_id)
        return ActionResult(
            action_type="inject", success=True,
            detail={"memory_ids": [m.memory_id for m in stored], "count": len(stored)},
        )

    def _do_inject(self, user_id: str, spec: dict, *, session_id: str | None = None) -> ActionResult:
        from core.memory.types import MemoryType, TrustTier

        content = spec.get("content")
        if not content:
            return ActionResult(action_type="inject", success=False,
                                error="inject requires 'content'")

        _TYPE_ALIASES = {"preference": "profile", "fact": "semantic", "skill": "procedural"}
        raw_type = spec.get("type", "semantic")
        mem_type = MemoryType(_TYPE_ALIASES.get(raw_type, raw_type))

        raw_trust = spec.get("trust", "T2")
        if isinstance(raw_trust, (int, float)):
            raw_trust = "T1" if raw_trust >= 0.9 else "T2" if raw_trust >= 0.7 else "T3" if raw_trust >= 0.4 else "T4"
        trust = TrustTier(raw_trust)

        mem = self._editor.inject(
            user_id, content,
            memory_type=mem_type,
            trust_tier=trust,
            session_id=session_id,
        )
        return ActionResult(
            action_type="inject", success=True,
            detail={"memory_id": mem.memory_id},
        )

    def _do_correct(self, user_id: str, spec: dict) -> ActionResult:
        memory_id = spec.get("memory_id")
        new_content = spec.get("new_content")
        if not memory_id or not new_content:
            return ActionResult(action_type="correct", success=False,
                                error="correct requires 'memory_id' and 'new_content'")

        mem = self._editor.correct(
            user_id, memory_id, new_content,
            reason=spec.get("reason", ""),
        )
        return ActionResult(
            action_type="correct", success=True,
            detail={"old_id": memory_id, "new_id": mem.memory_id},
        )

    def _do_purge(self, user_id: str, spec: dict) -> ActionResult:
        from datetime import datetime, timezone
        from core.memory.types import MemoryType

        filter_spec = spec.get("filter", {})
        memory_ids = filter_spec.get("memory_ids")
        type_val = filter_spec.get("type")
        memory_types = [MemoryType(type_val)] if type_val else None

        before: datetime | None = None
        if before_str := filter_spec.get("before"):
            before = datetime.fromisoformat(before_str).replace(tzinfo=timezone.utc) \
                if datetime.fromisoformat(before_str).tzinfo is None \
                else datetime.fromisoformat(before_str)

        result = self._editor.purge(
            user_id,
            memory_ids=memory_ids,
            memory_types=memory_types,
            before=before,
            reason=spec.get("reason", ""),
        )
        return ActionResult(
            action_type="purge", success=True,
            detail={"deactivated": result.deactivated,
                    "snapshot": result.snapshot_name},
        )

    def _do_tune(self, user_id: str, spec: dict) -> ActionResult:
        from core.memory.strategy.params import validate_strategy_params

        strategy = spec.get("strategy")
        params = spec.get("params")
        if not strategy:
            return ActionResult(action_type="tune", success=False,
                                error="tune requires 'strategy'")

        validated = validate_strategy_params(strategy, params)

        from core.memory.factory import set_user_strategy

        set_user_strategy(self._db_factory, user_id, strategy)

        if validated:
            from sqlalchemy import func as sa_func
            from core.memory.models.memory_config import MemoryUserConfig

            with self._db_factory() as db:
                db.query(MemoryUserConfig).filter_by(
                    user_id=user_id,
                ).update({"params_json": validated, "updated_at": sa_func.now()})
                db.commit()

        return ActionResult(
            action_type="tune", success=True,
            detail={"strategy": strategy, "params": validated},
        )


def nl_to_script(user_input: str, user_id: str, llm_client: Any, *, model: str | None = None) -> list[dict]:
    """Convert natural language instruction to structured actions via LLM."""
    from core.memory.programmer_prompts import NL_TO_SCRIPT_PROMPT

    prompt = NL_TO_SCRIPT_PROMPT.format(user_input=user_input, user_id=user_id)
    kwargs: dict = {
        "messages": [{"role": "user", "content": prompt}],
        "user_id": user_id,
        "task_hint": "memory_program_nl_convert",
        "temperature": 0.0,
        "model": model or "cheapest",
    }
    response = llm_client.chat(**kwargs)
    return parse_script(response.content)