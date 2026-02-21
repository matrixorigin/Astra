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


# ── Built-in: submit_dag ──

async def _execute_submit_dag(params: dict[str, Any], run_id: str | None = None) -> dict:
    """Submit a multi-step DAG. Steps run sequentially via JobBackend.

    Each step's output is passed as input to the next step.
    The run parks once and resumes only when the entire DAG completes.
    """
    import asyncio as _aio
    from uuid import uuid4

    from core.jobs.backend import JobRequirements
    from core.jobs.router import JobRouter

    dag_id = str(uuid4())[:12]
    steps = params["steps"]  # [{job_type, inputs?, ...}, ...]

    async def _run_dag() -> dict:
        router = JobRouter()
        carry: dict = {}
        results: list[dict] = []
        for i, step in enumerate(steps):
            merged_inputs = {**step.get("inputs", {}), **carry}
            req = JobRequirements(
                gpu_required=step.get("gpu_required", False),
                timeout_seconds=step.get("timeout_seconds", 3600),
                conda_env=step.get("conda_env"),
            )
            backend = router.select(req)
            job_id = await backend.submit(step["job_type"], merged_inputs, req)
            result = await backend.wait(job_id)
            step_out = {
                "step": i, "job_type": step["job_type"], "job_id": job_id,
                "status": result.status.value, "result": result.result, "error": result.error,
            }
            results.append(step_out)
            if result.status.value != "completed":
                return {"dag_id": dag_id, "status": "failed", "steps": results}
            carry = result.result or {}
        return {"dag_id": dag_id, "status": "completed", "steps": results}

    async def _dag_then_resolve() -> None:
        try:
            result = await _run_dag()
        except Exception as e:
            logger.error(f"DAG {dag_id} failed: {e}")
            result = {"dag_id": dag_id, "status": "failed", "error": str(e)}
        # Resolve handle — RunEngine.resolve_handle will resume the run
        reg = get_async_tool_registry()
        waiting_run_id = reg.resolve_handle(f"dag:{dag_id}")
        if waiting_run_id:
            # Import late to avoid circular
            from core.agent.run_engine import RunEngine
            from api.database import get_db_session
            db = next(get_db_session())
            try:
                engine = RunEngine(db)
                await engine.resume_run(waiting_run_id, result)
            finally:
                db.close()

    _aio.create_task(_dag_then_resolve())
    return {"dag_id": dag_id, "steps_count": len(steps), "status": "submitted", "wait_for": f"dag:{dag_id}"}


_SUBMIT_DAG_SCHEMA = {
    "type": "function",
    "function": {
        "name": "submit_dag",
        "description": (
            "Submit a multi-step pipeline (DAG) where each step's output feeds "
            "into the next. The agent run pauses until the entire pipeline completes. "
            "Use this when steps are predetermined and don't need LLM decisions between them."
        ),
        "parameters": {
            "type": "object",
            "properties": {
                "steps": {
                    "type": "array",
                    "description": "Ordered list of job steps. Each step's output is passed to the next.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "job_type": {"type": "string"},
                            "inputs": {"type": "object"},
                            "gpu_required": {"type": "boolean", "default": False},
                            "timeout_seconds": {"type": "integer", "default": 3600},
                            "conda_env": {"type": "string"},
                        },
                        "required": ["job_type"],
                    },
                },
            },
            "required": ["steps"],
        },
    },
}

_registry.register("submit_dag", _execute_submit_dag, _SUBMIT_DAG_SCHEMA)


# ── Backward compat (used by jobs webhook + old imports) ──

SUBMIT_JOB_SCHEMA = _SUBMIT_JOB_SCHEMA


def get_job_to_run_map() -> dict[str, str]:
    """Deprecated — use get_async_tool_registry().resolve_handle() instead."""
    return _registry._handle_to_run
