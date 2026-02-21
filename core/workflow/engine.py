"""Lightweight workflow engine — JSON-serializable, LLM-generable.

Supports: sequential, parallel, condition, wait (human/external),
sub-workflow, retry, timeout, loop, cancel propagation.
Zero external dependencies. Runs on top of JobBackend.

Workflow DSL (Pydantic models → JSON ↔ DB ↔ LLM):

    Workflow(
        name="train_pipeline",
        steps=[
            Step(id="collect", type="job", job_type="collect_corpus"),
            Step(id="clean", type="job", job_type="clean_data",
                 inputs_from=["collect"], retry=2),
            Step(id="compare", type="parallel", branches=[
                Step(id="gpu", type="job", job_type="train_gpu"),
                Step(id="cpu", type="job", job_type="train_cpu"),
            ]),
            Step(id="gate", type="condition",
                 expr="steps.compare.gpu.accuracy > 0.9",
                 then_step="publish", else_step="review"),
            Step(id="publish", type="job", job_type="publish_model"),
            Step(id="review", type="wait", wait_for="approval"),
        ],
    )
"""

from __future__ import annotations

import asyncio
import logging
import operator
import re
from datetime import datetime, timezone
from enum import Enum
from typing import Any

from pydantic import BaseModel, Field

logger = logging.getLogger(__name__)


# ── DSL Models ──


class StepType(str, Enum):
    JOB = "job"
    PARALLEL = "parallel"
    CONDITION = "condition"
    WAIT = "wait"
    WORKFLOW = "workflow"
    LOOP = "loop"  # repeat body until condition


class Step(BaseModel):
    id: str
    type: StepType

    # job
    job_type: str | None = None
    inputs: dict[str, Any] = Field(default_factory=dict)
    inputs_from: list[str] | str | None = None
    gpu_required: bool = False
    timeout_seconds: float = 3600
    conda_env: str | None = None
    retry: int = 0  # max retry count on failure

    # parallel
    branches: list[Step] | None = None

    # condition
    expr: str | None = None
    then_step: str | None = None
    else_step: str | None = None

    # wait
    wait_for: str | None = None

    # workflow (nested)
    workflow_ref: str | None = None

    # loop
    body: list[Step] | None = None  # steps to repeat
    until: str | None = None  # condition to stop (evaluated after each iteration)
    max_iterations: int = 10


class Workflow(BaseModel):
    name: str
    steps: list[Step]
    description: str = ""
    timeout_seconds: float = 0  # 0 = no limit


# ── Execution State ──


class StepStatus(str, Enum):
    PENDING = "pending"
    RUNNING = "running"
    COMPLETED = "completed"
    FAILED = "failed"
    SKIPPED = "skipped"
    WAITING = "waiting"


class StepResult(BaseModel):
    step_id: str
    status: StepStatus
    output: dict[str, Any] = Field(default_factory=dict)
    error: str | None = None
    started_at: str | None = None
    completed_at: str | None = None


class WorkflowRun(BaseModel):
    """Runtime state — fully serializable for persistence."""
    workflow_name: str
    step_results: dict[str, StepResult] = Field(default_factory=dict)
    current_step_idx: int = 0
    status: str = "running"
    waiting_for: str | None = None
    waiting_step_id: str | None = None
    started_at: str = Field(default_factory=lambda: datetime.now(timezone.utc).isoformat())
    error: str | None = None


# ── Progress callback ──

ProgressCallback = Any  # Callable[[str, StepResult], None] | None


# ── Engine ──


class WorkflowEngine:
    """Execute a Workflow definition. Stateless — all state in WorkflowRun."""

    def __init__(self, on_progress: ProgressCallback = None) -> None:
        self._registered: dict[str, Workflow] = {}
        self._on_progress = on_progress
        self._cancelled: set[str] = set()  # workflow names being cancelled
        self._active_jobs: dict[str, list[str]] = {}  # wf_name → [job_ids]

    def register(self, workflow: Workflow) -> None:
        self._registered[workflow.name] = workflow

    def get(self, name: str) -> Workflow | None:
        return self._registered.get(name)

    def cancel(self, workflow_name: str) -> None:
        self._cancelled.add(workflow_name)

    async def execute(
        self, workflow: Workflow, initial_inputs: dict | None = None,
    ) -> WorkflowRun:
        run = WorkflowRun(workflow_name=workflow.name)
        if initial_inputs:
            run.step_results["_initial"] = StepResult(
                step_id="_initial", status=StepStatus.COMPLETED, output=initial_inputs,
            )

        coro = self._execute_loop(workflow, run)
        if workflow.timeout_seconds > 0:
            try:
                await asyncio.wait_for(coro, timeout=workflow.timeout_seconds)
            except asyncio.TimeoutError:
                run.status = "failed"
                run.error = f"Workflow timeout after {workflow.timeout_seconds}s"
        else:
            await coro
        return run

    async def resume(
        self, workflow: Workflow, run: WorkflowRun, event_result: dict,
    ) -> WorkflowRun:
        if run.waiting_step_id:
            run.step_results[run.waiting_step_id] = StepResult(
                step_id=run.waiting_step_id, status=StepStatus.COMPLETED, output=event_result,
            )
        run.current_step_idx += 1
        run.status = "running"
        run.waiting_for = None
        run.waiting_step_id = None
        await self._execute_loop(workflow, run)
        return run

    # ── Core loop ──

    async def _execute_loop(self, workflow: Workflow, run: WorkflowRun) -> None:
        # Build step index for jump targets
        step_index = {s.id: i for i, s in enumerate(workflow.steps)}

        while run.current_step_idx < len(workflow.steps):
            if workflow.name in self._cancelled:
                run.status = "cancelled"
                self._cancelled.discard(workflow.name)
                return

            step = workflow.steps[run.current_step_idx]
            result = await self._execute_step_with_retry(step, run, workflow)
            run.step_results[step.id] = result
            self._report(workflow.name, result)

            if result.status == StepStatus.WAITING:
                run.status = "waiting"
                run.waiting_for = result.error  # handle stored in error field
                run.waiting_step_id = step.id
                return

            if result.status == StepStatus.FAILED:
                run.status = "failed"
                run.error = f"Step {step.id} failed: {result.error}"
                return

            # Handle condition jumps
            if step.type == StepType.CONDITION and result.output.get("jump_to"):
                target = result.output["jump_to"]
                if target in step_index:
                    # Mark skipped steps between current and target
                    target_idx = step_index[target]
                    for skip_idx in range(run.current_step_idx + 1, target_idx):
                        skip_step = workflow.steps[skip_idx]
                        if skip_step.id not in run.step_results:
                            run.step_results[skip_step.id] = StepResult(
                                step_id=skip_step.id, status=StepStatus.SKIPPED,
                            )
                    run.current_step_idx = target_idx
                    continue

            run.current_step_idx += 1

        run.status = "completed"

    # ── Step execution with retry ──

    async def _execute_step_with_retry(
        self, step: Step, run: WorkflowRun, workflow: Workflow,
    ) -> StepResult:
        last_result: StepResult | None = None
        for attempt in range(step.retry + 1):
            result = await self._execute_step(step, run, workflow)
            if result.status != StepStatus.FAILED or attempt >= step.retry:
                return result
            last_result = result
            logger.warning(f"Step {step.id} failed (attempt {attempt + 1}/{step.retry + 1}), retrying...")
            await asyncio.sleep(min(2 ** attempt, 30))  # exponential backoff, cap 30s
        return last_result  # type: ignore[return-value]

    async def _execute_step(
        self, step: Step, run: WorkflowRun, workflow: Workflow,
    ) -> StepResult:
        now = datetime.now(timezone.utc).isoformat()
        try:
            match step.type:
                case StepType.JOB:
                    return await self._exec_job(step, run, now)
                case StepType.PARALLEL:
                    return await self._exec_parallel(step, run, workflow, now)
                case StepType.CONDITION:
                    return self._exec_condition(step, run, now)
                case StepType.WAIT:
                    return StepResult(
                        step_id=step.id, status=StepStatus.WAITING,
                        error=step.wait_for or f"wait:{step.id}",
                        started_at=now,
                    )
                case StepType.WORKFLOW:
                    return await self._exec_sub_workflow(step, run, now)
                case StepType.LOOP:
                    return await self._exec_loop(step, run, workflow, now)
                case _:
                    return StepResult(
                        step_id=step.id, status=StepStatus.FAILED,
                        error=f"Unknown type: {step.type}", started_at=now,
                    )
        except Exception as e:
            logger.error(f"Step {step.id} failed: {e}", exc_info=True)
            return StepResult(
                step_id=step.id, status=StepStatus.FAILED,
                error=str(e), started_at=now,
            )

    # ── Step type executors ──

    async def _exec_job(self, step: Step, run: WorkflowRun, now: str) -> StepResult:
        from core.jobs.backend import JobRequirements
        from core.jobs.router import JobRouter

        inputs = self._resolve_inputs(step, run)
        req = JobRequirements(
            gpu_required=step.gpu_required,
            timeout_seconds=step.timeout_seconds,
            conda_env=step.conda_env,
        )
        backend = JobRouter().select(req)
        job_id = await backend.submit(step.job_type, inputs, req)
        self._active_jobs.setdefault(run.workflow_name, []).append(job_id)

        try:
            timeout = step.timeout_seconds if step.timeout_seconds > 0 else None
            result = await asyncio.wait_for(backend.wait(job_id), timeout=timeout)
        except asyncio.TimeoutError:
            return StepResult(
                step_id=step.id, status=StepStatus.FAILED,
                error=f"Job {job_id} timed out after {step.timeout_seconds}s",
                started_at=now,
            )
        done = datetime.now(timezone.utc).isoformat()

        if result.status.value == "completed":
            return StepResult(
                step_id=step.id, status=StepStatus.COMPLETED,
                output=result.result or {}, started_at=now, completed_at=done,
            )
        return StepResult(
            step_id=step.id, status=StepStatus.FAILED,
            error=result.error, started_at=now, completed_at=done,
        )

    async def _exec_parallel(
        self, step: Step, run: WorkflowRun, workflow: Workflow, now: str,
    ) -> StepResult:
        if not step.branches:
            return StepResult(step_id=step.id, status=StepStatus.COMPLETED, started_at=now)

        tasks = [self._execute_step(b, run, workflow) for b in step.branches]
        timeout = step.timeout_seconds if step.timeout_seconds > 0 else None
        try:
            results = await asyncio.wait_for(
                asyncio.gather(*tasks, return_exceptions=True), timeout=timeout,
            )
        except asyncio.TimeoutError:
            return StepResult(
                step_id=step.id, status=StepStatus.FAILED,
                error=f"Parallel step timed out after {step.timeout_seconds}s",
                started_at=now,
            )

        merged: dict[str, Any] = {}
        for branch, res in zip(step.branches, results):
            if isinstance(res, Exception):
                return StepResult(
                    step_id=step.id, status=StepStatus.FAILED,
                    error=str(res), started_at=now,
                )
            run.step_results[branch.id] = res
            merged[branch.id] = res.output
            if res.status == StepStatus.FAILED:
                return StepResult(
                    step_id=step.id, status=StepStatus.FAILED,
                    error=res.error, started_at=now,
                )

        return StepResult(
            step_id=step.id, status=StepStatus.COMPLETED,
            output=merged, started_at=now,
            completed_at=datetime.now(timezone.utc).isoformat(),
        )

    def _exec_condition(self, step: Step, run: WorkflowRun, now: str) -> StepResult:
        if not step.expr:
            return StepResult(step_id=step.id, status=StepStatus.FAILED, error="No expr")
        result = _safe_eval(step.expr, run.step_results)
        jump_to = step.then_step if result else step.else_step
        return StepResult(
            step_id=step.id, status=StepStatus.COMPLETED,
            output={"expr_result": result, "jump_to": jump_to}, started_at=now,
        )

    async def _exec_sub_workflow(self, step: Step, run: WorkflowRun, now: str) -> StepResult:
        sub = self._registered.get(step.workflow_ref or "")
        if not sub:
            return StepResult(
                step_id=step.id, status=StepStatus.FAILED,
                error=f"Workflow not found: {step.workflow_ref}", started_at=now,
            )
        inputs = self._resolve_inputs(step, run)
        sub_run = await self.execute(sub, initial_inputs=inputs)
        if sub_run.status == "completed":
            output = {sid: sr.output for sid, sr in sub_run.step_results.items() if sid != "_initial"}
            return StepResult(step_id=step.id, status=StepStatus.COMPLETED, output=output, started_at=now)
        return StepResult(
            step_id=step.id, status=StepStatus.FAILED,
            error=f"Sub-workflow {sub.name}: {sub_run.status} - {sub_run.error}", started_at=now,
        )

    async def _exec_loop(
        self, step: Step, run: WorkflowRun, workflow: Workflow, now: str,
    ) -> StepResult:
        """Execute body steps repeatedly until `until` condition is true."""
        if not step.body:
            return StepResult(step_id=step.id, status=StepStatus.COMPLETED, started_at=now)

        for iteration in range(step.max_iterations):
            # Execute body steps sequentially
            for body_step in step.body:
                body_result = await self._execute_step_with_retry(body_step, run, workflow)
                run.step_results[body_step.id] = body_result
                self._report(workflow.name, body_result)
                if body_result.status == StepStatus.FAILED:
                    return StepResult(
                        step_id=step.id, status=StepStatus.FAILED,
                        error=f"Loop body step {body_step.id} failed: {body_result.error}",
                        started_at=now,
                    )

            # Check until condition
            if step.until and _safe_eval(step.until, run.step_results):
                return StepResult(
                    step_id=step.id, status=StepStatus.COMPLETED,
                    output={"iterations": iteration + 1}, started_at=now,
                    completed_at=datetime.now(timezone.utc).isoformat(),
                )

        return StepResult(
            step_id=step.id, status=StepStatus.FAILED,
            error=f"Loop exceeded max_iterations ({step.max_iterations})", started_at=now,
        )

    # ── Data flow ──

    def _resolve_inputs(self, step: Step, run: WorkflowRun) -> dict:
        resolved = dict(step.inputs)
        sources = step.inputs_from or []
        if isinstance(sources, str):
            sources = [sources]
        for src_id in sources:
            src = run.step_results.get(src_id)
            if src and src.output:
                resolved.update(src.output)
        # Also include _initial inputs
        initial = run.step_results.get("_initial")
        if initial and initial.output and not sources:
            resolved.update(initial.output)
        return resolved

    def _report(self, wf_name: str, result: StepResult) -> None:
        if self._on_progress:
            try:
                self._on_progress(wf_name, result)
            except Exception:
                pass


# ── Safe expression evaluator (no eval/exec) ──

_OPS = {
    ">": operator.gt, "<": operator.lt, ">=": operator.ge, "<=": operator.le,
    "==": operator.eq, "!=": operator.ne,
}
_EXPR_RE = re.compile(r"^([\w.]+)\s*(>=|<=|!=|==|>|<)\s*(.+)$")


def _safe_eval(expr: str, step_results: dict[str, StepResult]) -> bool:
    m = _EXPR_RE.match(expr.strip())
    if not m:
        logger.warning(f"Cannot parse expr: {expr}")
        return False
    path, op_str, rhs_str = m.group(1), m.group(2), m.group(3).strip()
    lhs = _resolve_path(path, step_results)
    if lhs is None:
        return False
    rhs: Any
    try:
        rhs = float(rhs_str)
    except ValueError:
        rhs = rhs_str.strip("'\"")
    return _OPS[op_str](lhs, rhs)


def _resolve_path(path: str, step_results: dict[str, StepResult]) -> Any:
    parts = path.split(".")
    if parts[0] == "steps":
        parts = parts[1:]
    if not parts:
        return None
    sr = step_results.get(parts[0])
    if not sr:
        return None
    obj: Any = sr.output
    for key in parts[1:]:
        if isinstance(obj, dict):
            obj = obj.get(key)
        else:
            obj = getattr(obj, key, None)
        if obj is None:
            return None
    return obj
