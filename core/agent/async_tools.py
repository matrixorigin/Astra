"""Async tools — extensible registry for tools that park agent runs.

Any tool that returns `wait_for` in its result will cause the run to park
until the corresponding handle is resolved via `resolve_handle()`.

To add a new async tool:
    1. Write an async execute function: async def my_tool(params, run_id) -> dict
       - Must return {"wait_for": "<type>:<id>", ...} to park
    2. Define an OpenAI function-calling schema dict
    3. Register: async_tool_registry.register("my_tool", execute_fn, schema)
    4. Resolve when done: async_tool_registry.resolve_handle("type:id", result)
"""

from __future__ import annotations

import logging
from collections.abc import Awaitable, Callable
from typing import Any

from core.utils.id_generator import generate_id

logger = logging.getLogger(__name__)

# Type alias for async tool executors
AsyncToolExecutor = Callable[[dict[str, Any], str | None], Awaitable[dict]]


class AsyncToolRegistry:
    """Registry of async tools that can park agent runs."""

    def __init__(self) -> None:
        self._executors: dict[str, AsyncToolExecutor] = {}
        self._schemas: dict[str, dict] = {}
        # handle → run_id mapping (e.g. "job:abc123" → "run_xyz")
        self._handle_to_run: dict[str, str] = {}

    def register(self, name: str, executor: AsyncToolExecutor, schema: dict) -> None:
        self._executors[name] = executor
        self._schemas[name] = schema

    def is_async_tool(self, name: str) -> bool:
        return name in self._executors

    async def execute(self, name: str, params: dict, run_id: str | None = None) -> dict:
        """Execute an async tool. Links wait_for handle to run_id."""
        executor = self._executors[name]
        result = await executor(params, run_id)
        # Auto-track handle → run mapping
        if run_id and result.get("wait_for"):
            self._handle_to_run[result["wait_for"]] = run_id
            logger.info(f"Handle {result['wait_for']} linked to run {run_id}")
        return result

    def resolve_handle(self, handle: str) -> str | None:
        """Pop and return the run_id waiting for this handle, or None."""
        return self._handle_to_run.pop(handle, None)

    def get_schemas(self) -> list[dict]:
        return list(self._schemas.values())

    @property
    def tool_names(self) -> set[str]:
        return set(self._executors.keys())


# ── Singleton ──

_registry = AsyncToolRegistry()


def get_async_tool_registry() -> AsyncToolRegistry:
    return _registry


# ── Built-in: submit_job ──


async def _execute_submit_job(params: dict[str, Any], run_id: str | None = None) -> dict:
    from core.jobs.backend import JobRequirements
    from core.jobs.router import JobRouter

    job_type = params["job_type"]
    inputs = params.get("inputs", {})
    req = JobRequirements(
        gpu_required=params.get("gpu_required", False),
        timeout_seconds=params.get("timeout_seconds", 3600),
        conda_env=params.get("conda_env"),
    )
    backend = JobRouter().select(req)
    job_id = await backend.submit(job_type, inputs, req)
    return {"job_id": job_id, "status": "submitted", "wait_for": f"job:{job_id}"}


_SUBMIT_JOB_SCHEMA = {
    "type": "function",
    "function": {
        "name": "submit_job",
        "description": (
            "Submit a background job (training, data collection, etc). "
            "The agent run will pause until the job completes."
        ),
        "parameters": {
            "type": "object",
            "properties": {
                "job_type": {
                    "type": "string",
                    "description": "Job type (e.g. train_model, collect_corpus, run_pipeline)",
                },
                "inputs": {
                    "type": "object",
                    "description": "Job-specific input parameters",
                },
                "gpu_required": {"type": "boolean", "default": False},
                "timeout_seconds": {"type": "integer", "default": 3600},
                "conda_env": {"type": "string", "description": "Conda env to run in"},
            },
            "required": ["job_type"],
        },
    },
}

# Auto-register built-in async tools
_registry.register("submit_job", _execute_submit_job, _SUBMIT_JOB_SCHEMA)


# ── Built-in: submit_workflow ──


async def _execute_submit_workflow(params: dict[str, Any], run_id: str | None = None) -> dict:
    """Submit a workflow. Persists to wf_definitions + wf_runs tables."""
    import asyncio as _aio

    from core.workflow.engine import Workflow, WorkflowEngine, Step

    wf_id = generate_id()
    steps = [Step(**s) if isinstance(s, dict) else s for s in params["steps"]]
    workflow = Workflow(
        name=params.get("name", f"wf_{wf_id}"),
        steps=steps,
        timeout_seconds=params.get("timeout_seconds", 0),
    )
    initial_inputs = params.get("inputs", {})

    # Persist workflow definition + create run record
    _persist_workflow_start(wf_id, workflow, initial_inputs, run_id)

    # In-memory state for resume
    _wf_runs[wf_id] = {
        "workflow": workflow,
        "engine": WorkflowEngine(wf_run_id=wf_id),
        "wf_run": None,
    }

    async def _run_then_resolve() -> None:
        entry = _wf_runs[wf_id]
        engine = entry["engine"]
        try:
            wf_run = await engine.execute(workflow, initial_inputs=initial_inputs)
            entry["wf_run"] = wf_run
            try:
                _persist_workflow_state(wf_id, wf_run)
            except Exception as pe:
                logger.error(f"Workflow {wf_id} persist failed (non-fatal): {pe}")

            if wf_run.status == "waiting":
                inner_handle = wf_run.waiting_for or f"wait:{wf_id}"
                _workflow_waits[inner_handle] = wf_id
                logger.info(f"Workflow {wf_id} waiting on {inner_handle}")
                return

            result = _workflow_result(wf_id, wf_run)
        except Exception as e:
            logger.error(f"Workflow {wf_id} failed: {e}")
            result = {"workflow_id": wf_id, "status": "failed", "error": str(e)}

        _resolve_workflow(wf_id, result)

    _aio.create_task(_run_then_resolve())
    return {"workflow_id": wf_id, "status": "submitted", "wait_for": f"workflow:{wf_id}"}


# Workflow state for resume
_wf_runs: dict[str, dict] = {}  # wf_id → {workflow, engine, wf_run}
_workflow_waits: dict[str, str] = {}  # inner_handle → wf_id


async def resume_workflow(inner_handle: str, event_result: dict) -> bool:
    """Resume a workflow that's waiting on an inner wait step. Distributed-safe."""
    wf_id = _workflow_waits.pop(inner_handle, None)

    # DB fallback: handle might be on another worker
    if not wf_id:
        wf_id = _find_workflow_by_wait_handle(inner_handle)

    if not wf_id:
        return False

    entry = _wf_runs.get(wf_id)

    # DB fallback: workflow might be on another worker — restore it
    if not entry or not entry.get("wf_run"):
        entry = _restore_workflow_entry(wf_id)
        if not entry:
            return False
        _wf_runs[wf_id] = entry

    engine = entry["engine"]
    workflow = entry["workflow"]
    wf_run = entry["wf_run"]

    try:
        wf_run = await engine.resume(workflow, wf_run, event_result)
        entry["wf_run"] = wf_run
        _persist_workflow_state(wf_id, wf_run)

        if wf_run.status == "waiting":
            inner = wf_run.waiting_for or f"wait:{wf_id}"
            _workflow_waits[inner] = wf_id
            return True

        result = _workflow_result(wf_id, wf_run)
    except Exception as e:
        result = {"workflow_id": wf_id, "status": "failed", "error": str(e)}

    _resolve_workflow(wf_id, result)
    return True


def _find_workflow_by_wait_handle(handle: str) -> str | None:
    """Find workflow waiting for this handle from DB."""
    try:
        from api.database import get_db_session
        from api.models import WorkflowRun as WFRunModel

        db = next(get_db_session())
        try:
            row = (
                db.query(WFRunModel)
                .filter(
                    WFRunModel.status == "waiting",
                    WFRunModel.waiting_for == handle,
                )
                .first()
            )
            return row.run_id if row else None
        finally:
            db.close()
    except Exception:
        return None


def _restore_workflow_entry(wf_id: str) -> dict | None:
    """Restore a workflow's in-memory entry from DB."""
    try:
        from api.database import get_db_session
        from api.models import WorkflowRun as WFRunModel, WorkflowDefinition
        from core.workflow.engine import Workflow, WorkflowEngine
        from core.workflow.engine import WorkflowRun as WFRunState, StepResult

        db = next(get_db_session())
        try:
            row = db.query(WFRunModel).filter(WFRunModel.run_id == wf_id).first()
            if not row:
                return None
            wf_def = (
                db.query(WorkflowDefinition)
                .filter(
                    WorkflowDefinition.workflow_id == row.workflow_id,
                )
                .first()
            )
            if not wf_def:
                return None

            workflow = Workflow(**wf_def.definition)
            wf_run = WFRunState(
                workflow_name=workflow.name,
                current_step_idx=row.current_step_idx,
                status=row.status,
                waiting_for=row.waiting_for,
                waiting_step_id=row.waiting_step_id,
            )
            for sid, sr_data in (row.step_results or {}).items():
                wf_run.step_results[sid] = StepResult(**sr_data)

            return {"workflow": workflow, "engine": WorkflowEngine(), "wf_run": wf_run}
        finally:
            db.close()
    except Exception as e:
        logger.error(f"Failed to restore workflow {wf_id}: {e}")
        return None


def _workflow_result(wf_id: str, wf_run) -> dict:
    return {
        "workflow_id": wf_id,
        "status": wf_run.status,
        "steps": {
            sid: sr.model_dump() for sid, sr in wf_run.step_results.items() if sid != "_initial"
        },
    }


def _resolve_workflow(wf_id: str, result: dict) -> None:
    """Resolve the workflow handle to resume the agent run."""
    import asyncio as _aio

    reg = get_async_tool_registry()
    waiting_run_id = reg.resolve_handle(f"workflow:{wf_id}")
    if waiting_run_id:

        async def _do_resume():
            from core.agent.run_engine import RunEngine
            from api.database import SessionLocal

            await RunEngine(SessionLocal).resume_run(waiting_run_id, result)

        _aio.create_task(_do_resume())
    _wf_runs.pop(wf_id, None)


async def cleanup_stale_workflows(max_age_hours: int = 24) -> int:
    """Clean up workflows stuck in waiting/running beyond max_age_hours.

    Marks them as 'failed' in DB, removes from in-memory dicts.
    Call periodically (e.g. from a background task or cron).
    Returns count of cleaned workflows.
    """
    from datetime import datetime, timezone, timedelta

    try:
        from api.database import get_db_session
        from api.models import WorkflowRun as WFRunModel

        db = next(get_db_session())
        try:
            cutoff = datetime.now(timezone.utc) - timedelta(hours=max_age_hours)
            stale = (
                db.query(WFRunModel)
                .filter(
                    WFRunModel.status.in_(["running", "waiting"]),
                    WFRunModel.started_at < cutoff,
                )
                .all()
            )
            count = 0
            for row in stale:
                row.status = "failed"
                row.error = f"Timed out after {max_age_hours}h"
                row.completed_at = datetime.now(timezone.utc)
                # Clean in-memory state
                _wf_runs.pop(row.run_id, None)
                if row.waiting_for:
                    _workflow_waits.pop(row.waiting_for, None)
                count += 1
            db.commit()
            if count:
                logger.info(f"Cleaned {count} stale workflows (>{max_age_hours}h)")
            return count
        finally:
            db.close()
    except Exception as e:
        logger.error(f"Workflow cleanup failed: {e}")
        return 0


def _persist_workflow_state(wf_id: str, wf_run) -> None:
    """Update wf_runs table with current state."""
    try:
        from api.database import get_db_session
        from api.models import WorkflowRun as WFRunModel

        db = next(get_db_session())
        try:
            row = db.query(WFRunModel).filter(WFRunModel.run_id == wf_id).first()
            if row:
                row.status = wf_run.status
                row.current_step_idx = wf_run.current_step_idx
                row.step_results = {
                    sid: sr.model_dump()
                    for sid, sr in wf_run.step_results.items()
                    if sid != "_initial"
                }
                row.waiting_for = wf_run.waiting_for
                row.waiting_step_id = wf_run.waiting_step_id
                row.error = wf_run.error
                if wf_run.status in ("completed", "failed", "cancelled"):
                    from datetime import datetime, timezone

                    row.completed_at = datetime.now(timezone.utc)
                db.commit()
        finally:
            db.close()
    except Exception as e:
        logger.error(f"Failed to persist workflow run {wf_id}: {e}")


def _persist_workflow_start(
    wf_id: str, workflow, initial_inputs: dict, agent_run_id: str | None
) -> None:
    """Create wf_definitions + wf_runs records."""
    try:
        from api.database import get_db_session
        from api.models import WorkflowDefinition, WorkflowRun as WFRunModel

        db = next(get_db_session())
        try:
            # Upsert definition
            def_id = f"{workflow.name}@latest"
            existing = (
                db.query(WorkflowDefinition)
                .filter(WorkflowDefinition.workflow_id == def_id)
                .first()
            )
            if not existing:
                db.add(
                    WorkflowDefinition(
                        workflow_id=def_id,
                        name=workflow.name,
                        version="latest",
                        definition=workflow.model_dump(),
                    )
                )
            # Create run
            db.add(
                WFRunModel(
                    run_id=wf_id,
                    workflow_id=def_id,
                    agent_run_id=agent_run_id,
                    status="running",
                    inputs=initial_inputs,
                )
            )
            db.commit()
        finally:
            db.close()
    except Exception as e:
        logger.error(f"Failed to persist workflow start {wf_id}: {e}")


_SUBMIT_WORKFLOW_SCHEMA = {
    "type": "function",
    "function": {
        "name": "submit_workflow",
        "description": (
            "Submit a multi-step workflow. Supports sequential, parallel, "
            "conditional branching, wait (human approval), and sub-workflows. "
            "The agent run pauses until the workflow completes. "
            "Use when steps are predetermined and don't need LLM decisions between them."
        ),
        "parameters": {
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "Workflow name"},
                "inputs": {"type": "object", "description": "Initial inputs"},
                "steps": {
                    "type": "array",
                    "description": "Workflow steps",
                    "items": {
                        "type": "object",
                        "properties": {
                            "id": {"type": "string", "description": "Unique step id"},
                            "type": {
                                "type": "string",
                                "enum": ["job", "parallel", "condition", "wait", "workflow"],
                            },
                            "job_type": {"type": "string", "description": "For type=job"},
                            "inputs": {"type": "object"},
                            "inputs_from": {
                                "description": "Step id(s) whose output feeds into this step",
                            },
                            "gpu_required": {"type": "boolean", "default": False},
                            "timeout_seconds": {"type": "integer", "default": 3600},
                            "branches": {
                                "type": "array",
                                "description": "For type=parallel: list of sub-steps to run concurrently",
                            },
                            "expr": {
                                "type": "string",
                                "description": "For type=condition: e.g. 'steps.train.accuracy > 0.9'",
                            },
                            "then_step": {
                                "type": "string",
                                "description": "Step id if condition is true",
                            },
                            "else_step": {
                                "type": "string",
                                "description": "Step id if condition is false",
                            },
                            "wait_for": {
                                "type": "string",
                                "description": "For type=wait: event to wait for",
                            },
                        },
                        "required": ["id", "type"],
                    },
                },
            },
            "required": ["steps"],
        },
    },
}

_registry.register("submit_workflow", _execute_submit_workflow, _SUBMIT_WORKFLOW_SCHEMA)


# ── Built-in: spawn_runs (multi-agent fan-out) ──


async def _execute_spawn_runs(params: dict[str, Any], run_id: str | None = None) -> dict:
    """Spawn child agent runs. Parent parks until all children complete."""
    if not run_id:
        return {"error": "spawn_runs requires a parent run_id"}

    from core.agent.run_engine import RunEngine, _active_runs

    parent = _active_runs.get(run_id)
    if not parent:
        return {"error": f"Parent run {run_id} not found in active runs"}

    from api.database import SessionLocal

    engine = RunEngine(SessionLocal)

    try:
        agents = params.get("agents")
        if not agents or not isinstance(agents, list):
            return {"error": "spawn_runs requires a non-empty 'agents' list"}

        children = []
        for spec in agents:
            if not isinstance(spec, dict) or "task" not in spec:
                continue  # Skip malformed entries
            child = await engine.create_child_run(
                parent_run_id=run_id,
                agent_id=spec.get("agent_id", "dev-agent"),
                task=spec["task"],
                context=spec.get("context"),
            )
            children.append({"run_id": child.run_id, "agent_id": child.agent_id})

        return {
            "children": children,
            "count": len(children),
            "wait_for": f"children:{run_id}",
        }
    except Exception as e:
        return {"error": str(e)}


_SPAWN_RUNS_SCHEMA = {
    "type": "function",
    "function": {
        "name": "spawn_runs",
        "description": (
            "Spawn multiple child agent runs in parallel (fan-out). "
            "The current run pauses until ALL children complete (fan-in). "
            "Use for multi-agent review, parallel analysis, etc."
        ),
        "parameters": {
            "type": "object",
            "properties": {
                "agents": {
                    "type": "array",
                    "description": "List of agent specs to spawn",
                    "items": {
                        "type": "object",
                        "properties": {
                            "agent_id": {
                                "type": "string",
                                "description": "Agent to run (e.g. security_reviewer)",
                            },
                            "task": {
                                "type": "string",
                                "description": "Task description for the agent",
                            },
                            "context": {
                                "type": "object",
                                "description": "Optional context for the agent",
                            },
                        },
                        "required": ["task"],
                    },
                },
            },
            "required": ["agents"],
        },
    },
}

_registry.register("spawn_runs", _execute_spawn_runs, _SPAWN_RUNS_SCHEMA)


# ── Backward compat (used by jobs webhook + old imports) ──

SUBMIT_JOB_SCHEMA = _SUBMIT_JOB_SCHEMA


def get_job_to_run_map() -> dict[str, str]:
    """Deprecated — use get_async_tool_registry().resolve_handle() instead."""
    return _registry._handle_to_run


def restore_waiting_workflows() -> int:
    """Restore in-memory state for workflows that were waiting when process died.

    Call on startup. Returns count of restored workflows.
    """
    try:
        from api.database import get_db_session
        from api.models import WorkflowRun as WFRunModel, WorkflowDefinition
        from core.workflow.engine import Workflow, WorkflowEngine
        from core.workflow.engine import WorkflowRun as WFRunState, StepResult

        db = next(get_db_session())
        try:
            restore_batch = 200
            rows = (
                db.query(WFRunModel)
                .filter(WFRunModel.status == "waiting")
                .limit(restore_batch)
                .all()
            )
            if len(rows) >= restore_batch:
                logger.warning(
                    "restore_waiting_workflows hit limit of %d; some workflows may not be restored",
                    restore_batch,
                )
            count = 0
            for row in rows:
                wf_def = (
                    db.query(WorkflowDefinition)
                    .filter(
                        WorkflowDefinition.workflow_id == row.workflow_id,
                    )
                    .first()
                )
                if not wf_def:
                    continue

                workflow = Workflow(**wf_def.definition)
                wf_run = WFRunState(
                    workflow_name=workflow.name,
                    current_step_idx=row.current_step_idx,
                    status=row.status,
                    waiting_for=row.waiting_for,
                    waiting_step_id=row.waiting_step_id,
                )
                # Restore step results
                for sid, sr_data in (row.step_results or {}).items():
                    wf_run.step_results[sid] = StepResult(**sr_data)

                _wf_runs[row.run_id] = {
                    "workflow": workflow,
                    "engine": WorkflowEngine(),
                    "wf_run": wf_run,
                }
                if row.waiting_for:
                    _workflow_waits[row.waiting_for] = row.run_id

                # Restore handle → agent_run mapping
                if row.agent_run_id:
                    _registry._handle_to_run[f"workflow:{row.run_id}"] = row.agent_run_id

                count += 1
                logger.info(f"Restored workflow {row.run_id} (waiting on {row.waiting_for})")
            return count
        finally:
            db.close()
    except Exception as e:
        logger.error(f"Failed to restore workflows: {e}")
        return 0
