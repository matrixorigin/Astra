"""ExecutionBackend — unified abstraction for skill execution routing.

Lightweight skills run in-process (zero overhead).
Heavyweight skills (GPU, conda, long timeout) route to subprocess/Ray/K8s.
"""

from __future__ import annotations

import asyncio
import json
import os
import sys
from abc import ABC, abstractmethod
from dataclasses import dataclass, field
from enum import Enum
from typing import Any
from core.utils.id_generator import generate_id

from core.logging_config import get_logger

logger = get_logger(__name__)


class ExecutionStatus(str, Enum):
    PENDING = "pending"
    RUNNING = "running"
    COMPLETED = "completed"
    FAILED = "failed"
    CANCELLED = "cancelled"


_TERMINAL = frozenset({ExecutionStatus.COMPLETED, ExecutionStatus.FAILED, ExecutionStatus.CANCELLED})


@dataclass
class ExecutionResult:
    job_id: str
    status: ExecutionStatus
    result: dict | None = None
    error: str | None = None


@dataclass
class ExecutionRequirements:
    """Resource requirements for skill execution."""
    gpu_required: bool = False
    conda_env: str | None = None
    timeout_seconds: int = 60
    min_memory_gb: float = 0.5
    env_vars: dict[str, str] = field(default_factory=dict)

    def __post_init__(self) -> None:
        if self.timeout_seconds < 1:
            self.timeout_seconds = 1


class ExecutionBackend(ABC):
    """Abstract backend for skill execution."""

    @abstractmethod
    async def submit(self, skill_name: str, inputs: dict, req: ExecutionRequirements) -> str:
        """Submit skill execution, return job_id."""

    @abstractmethod
    async def get_status(self, job_id: str) -> ExecutionResult:
        """Get execution status."""

    @abstractmethod
    async def cancel(self, job_id: str) -> bool:
        """Cancel execution."""

    @abstractmethod
    async def wait(self, job_id: str, timeout: float | None = None) -> ExecutionResult:
        """Wait for completion."""


class InProcessBackend(ExecutionBackend):
    """Execute skills in the current process. Zero overhead for lightweight skills."""

    _GC_THRESHOLD = 500

    def __init__(self) -> None:
        self._results: dict[str, ExecutionResult] = {}

    async def submit(self, skill_name: str, inputs: dict, req: ExecutionRequirements) -> str:
        job_id = generate_id()
        self._results[job_id] = ExecutionResult(job_id=job_id, status=ExecutionStatus.PENDING)
        self._maybe_gc()
        return job_id

    async def execute_sync(self, job_id: str, skill_fn, inputs: dict) -> ExecutionResult:
        """Run skill function directly. Called by BackendRouter for in-process execution."""
        self._results[job_id] = ExecutionResult(job_id=job_id, status=ExecutionStatus.RUNNING)
        try:
            result = await skill_fn(inputs) if asyncio.iscoroutinefunction(skill_fn) else skill_fn(inputs)
            r = ExecutionResult(
                job_id=job_id, status=ExecutionStatus.COMPLETED,
                result=result if isinstance(result, dict) else {"output": str(result)},
            )
        except Exception as e:
            r = ExecutionResult(job_id=job_id, status=ExecutionStatus.FAILED, error=str(e))
        self._results[job_id] = r
        return r

    async def get_status(self, job_id: str) -> ExecutionResult:
        return self._results.get(job_id, ExecutionResult(job_id=job_id, status=ExecutionStatus.FAILED, error="Unknown"))

    async def cancel(self, job_id: str) -> bool:
        return False  # In-process can't be cancelled mid-execution

    async def wait(self, job_id: str, timeout: float | None = None) -> ExecutionResult:
        return await self.get_status(job_id)

    def _maybe_gc(self) -> None:
        if len(self._results) <= self._GC_THRESHOLD:
            return
        done = [jid for jid, r in self._results.items() if r.status in _TERMINAL]
        for jid in done:
            self._results.pop(jid, None)


class SubprocessBackend(ExecutionBackend):
    """Execute skills as subprocesses with optional conda env."""

    _GC_THRESHOLD = 500

    def __init__(self) -> None:
        self._tasks: dict[str, asyncio.Task] = {}
        self._procs: dict[str, asyncio.subprocess.Process] = {}
        self._results: dict[str, ExecutionResult] = {}

    async def submit(self, skill_name: str, inputs: dict, req: ExecutionRequirements) -> str:
        job_id = generate_id()
        self._results[job_id] = ExecutionResult(job_id=job_id, status=ExecutionStatus.PENDING)
        self._tasks[job_id] = asyncio.create_task(self._run(job_id, skill_name, inputs, req))
        self._maybe_gc()
        return job_id

    async def get_status(self, job_id: str) -> ExecutionResult:
        return self._results.get(job_id, ExecutionResult(job_id=job_id, status=ExecutionStatus.FAILED, error="Unknown"))

    async def cancel(self, job_id: str) -> bool:
        task = self._tasks.get(job_id)
        if task and not task.done():
            proc = self._procs.get(job_id)
            if proc and proc.returncode is None:
                try:
                    proc.kill()
                except ProcessLookupError:
                    pass
            task.cancel()
            self._results[job_id] = ExecutionResult(job_id=job_id, status=ExecutionStatus.CANCELLED)
            return True
        return False

    async def wait(self, job_id: str, timeout: float | None = None) -> ExecutionResult:
        task = self._tasks.get(job_id)
        if task:
            try:
                await asyncio.wait_for(asyncio.shield(task), timeout=timeout)
            except asyncio.TimeoutError:
                pass
        return await self.get_status(job_id)

    def _maybe_gc(self) -> None:
        if len(self._results) <= self._GC_THRESHOLD:
            return
        done = [jid for jid, r in self._results.items() if r.status in _TERMINAL]
        for jid in done:
            self._results.pop(jid, None)
            self._tasks.pop(jid, None)
            self._procs.pop(jid, None)

    async def _run(self, job_id: str, skill_name: str, inputs: dict, req: ExecutionRequirements) -> None:
        self._results[job_id] = ExecutionResult(job_id=job_id, status=ExecutionStatus.RUNNING)
        proc = None
        try:
            cmd = [sys.executable, "-m", "core.skills.runner",
                   "--skill", skill_name, "--inputs", json.dumps(inputs)]
            if req.conda_env:
                cmd = ["conda", "run", "-n", req.conda_env, "--no-capture-output"] + cmd

            env = {**os.environ, **req.env_vars} if req.env_vars else None
            proc = await asyncio.create_subprocess_exec(
                *cmd, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE, env=env,
            )
            self._procs[job_id] = proc
            try:
                stdout, stderr = await asyncio.wait_for(proc.communicate(), timeout=req.timeout_seconds)
            except asyncio.TimeoutError:
                proc.kill()
                await proc.communicate()
                self._results[job_id] = ExecutionResult(job_id=job_id, status=ExecutionStatus.FAILED, error="Timeout")
                return

            if proc.returncode != 0:
                err_text = stderr.decode(errors="replace")[-2000:] if stderr else ""
                self._results[job_id] = ExecutionResult(
                    job_id=job_id, status=ExecutionStatus.FAILED, error=err_text)
            else:
                out_text = stdout.decode(errors="replace") if stdout else ""
                try:
                    result = json.loads(out_text) if out_text.strip() else {}
                except (json.JSONDecodeError, UnicodeDecodeError):
                    result = {"output": out_text[-2000:]}
                self._results[job_id] = ExecutionResult(
                    job_id=job_id, status=ExecutionStatus.COMPLETED, result=result)
        except asyncio.CancelledError:
            if proc and proc.returncode is None:
                try:
                    proc.kill()
                    await proc.communicate()
                except ProcessLookupError:
                    pass
            self._results[job_id] = ExecutionResult(job_id=job_id, status=ExecutionStatus.CANCELLED)
        except Exception as e:
            self._results[job_id] = ExecutionResult(job_id=job_id, status=ExecutionStatus.FAILED, error=str(e))


def _is_heavyweight(req: ExecutionRequirements) -> bool:
    """Single source of truth for lightweight/heavyweight classification."""
    return bool(req.gpu_required or req.conda_env or req.timeout_seconds > 300)


class BackendRouter:
    """Route skill execution to the appropriate backend.

    Lightweight skills → InProcessBackend (zero overhead)
    Heavyweight skills → SubprocessBackend (conda/GPU isolation)
    """

    def __init__(self) -> None:
        self.in_process = InProcessBackend()
        self.subprocess = SubprocessBackend()

    def select(self, req: ExecutionRequirements) -> ExecutionBackend:
        return self.subprocess if _is_heavyweight(req) else self.in_process

    def is_lightweight(self, req: ExecutionRequirements) -> bool:
        return not _is_heavyweight(req)
