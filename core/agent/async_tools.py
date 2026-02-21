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


# ── Backward compat (used by jobs webhook + old imports) ──

SUBMIT_JOB_SCHEMA = _SUBMIT_JOB_SCHEMA


def get_job_to_run_map() -> dict[str, str]:
    """Deprecated — use get_async_tool_registry().resolve_handle() instead."""
    return _registry._handle_to_run
