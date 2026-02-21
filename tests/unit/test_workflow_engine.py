"""Tests for workflow engine: execution, errors, timeouts, retry, lifecycle."""

import asyncio
import pytest
from unittest.mock import AsyncMock, MagicMock, patch

from core.workflow.engine import (
    Step, StepType, StepResult, StepStatus,
    Workflow, WorkflowEngine, WorkflowRun,
    _safe_eval,
)


# ── Helpers ──

def _job_step(id: str, job_type: str = "noop", **kw) -> Step:
    return Step(id=id, type=StepType.JOB, job_type=job_type, **kw)


def _wait_step(id: str, wait_for: str = "approval:1") -> Step:
    return Step(id=id, type=StepType.WAIT, wait_for=wait_for)


def _cond_step(id: str, expr: str, then_step: str = None, else_step: str = None) -> Step:
    return Step(id=id, type=StepType.CONDITION, expr=expr, then_step=then_step, else_step=else_step)


class FakeJobResult:
    def __init__(self, status="completed", result=None, error=None):
        self.status = MagicMock(value=status)
        self.result = result or {}
        self.error = error


class FakeBackend:
    def __init__(self, results=None, wait_delay=0):
        self._results = results or {}
        self._wait_delay = wait_delay
        self._submitted = []

    async def submit(self, job_type, inputs, req):
        job_id = f"job_{len(self._submitted)}"
        self._submitted.append((job_type, inputs))
        return job_id

    async def wait(self, job_id):
        if self._wait_delay:
            await asyncio.sleep(self._wait_delay)
        return self._results.get(job_id, FakeJobResult())


def _patch_job_router(backend):
    """Patch JobRouter to return our fake backend."""
    mock_router_cls = MagicMock(return_value=MagicMock(select=MagicMock(return_value=backend)))
    return patch("core.jobs.router.JobRouter", mock_router_cls)


# ── Basic execution ──

class TestWorkflowBasicExecution:

    @pytest.mark.asyncio
    async def test_empty_workflow(self):
        engine = WorkflowEngine()
        wf = Workflow(name="empty", steps=[])
        run = await engine.execute(wf)
        assert run.status == "completed"

    @pytest.mark.asyncio
    async def test_single_job_step(self):
        backend = FakeBackend(results={"job_0": FakeJobResult(result={"acc": 0.95})})
        engine = WorkflowEngine()
        wf = Workflow(name="single", steps=[_job_step("train", "train_model")])

        with _patch_job_router(backend):
            run = await engine.execute(wf)

        assert run.status == "completed"
        assert run.step_results["train"].output == {"acc": 0.95}

    @pytest.mark.asyncio
    async def test_sequential_steps_data_flow(self):
        backend = FakeBackend(results={
            "job_0": FakeJobResult(result={"model_path": "/m1"}),
            "job_1": FakeJobResult(result={"score": 0.9}),
        })
        engine = WorkflowEngine()
        wf = Workflow(name="seq", steps=[
            _job_step("train", "train"),
            _job_step("eval", "eval", inputs_from=["train"]),
        ])

        with _patch_job_router(backend):
            run = await engine.execute(wf)

        assert run.status == "completed"
        # eval step should have received train's output
        assert backend._submitted[1][1] == {"model_path": "/m1"}

    @pytest.mark.asyncio
    async def test_initial_inputs_flow(self):
        backend = FakeBackend(results={"job_0": FakeJobResult()})
        engine = WorkflowEngine()
        wf = Workflow(name="init", steps=[_job_step("s1", "noop")])

        with _patch_job_router(backend):
            run = await engine.execute(wf, initial_inputs={"lr": 0.01})

        # Step with no inputs_from should get initial inputs
        assert backend._submitted[0][1] == {"lr": 0.01}


# ── Wait / Resume ──

class TestWorkflowWaitResume:

    @pytest.mark.asyncio
    async def test_wait_step_parks_workflow(self):
        engine = WorkflowEngine()
        wf = Workflow(name="w", steps=[_wait_step("approve", "human:review")])
        run = await engine.execute(wf)

        assert run.status == "waiting"
        assert run.waiting_for == "human:review"
        assert run.waiting_step_id == "approve"

    @pytest.mark.asyncio
    async def test_resume_after_wait(self):
        engine = WorkflowEngine()
        backend = FakeBackend(results={"job_0": FakeJobResult(result={"done": True})})
        wf = Workflow(name="w", steps=[
            _wait_step("approve", "human:review"),
            _job_step("deploy", "deploy"),
        ])

        run = await engine.execute(wf)
        assert run.status == "waiting"

        with _patch_job_router(backend):
            run = await engine.resume(wf, run, {"approved": True})

        assert run.status == "completed"
        assert run.step_results["approve"].output == {"approved": True}
        assert run.step_results["deploy"].status == StepStatus.COMPLETED


# ── Error handling ──

class TestWorkflowErrors:

    @pytest.mark.asyncio
    async def test_job_failure_stops_workflow(self):
        backend = FakeBackend(results={
            "job_0": FakeJobResult(status="failed", error="OOM"),
        })
        engine = WorkflowEngine()
        wf = Workflow(name="fail", steps=[
            _job_step("train", "train"),
            _job_step("eval", "eval"),
        ])

        with _patch_job_router(backend):
            run = await engine.execute(wf)

        assert run.status == "failed"
        assert "OOM" in run.error
        assert "eval" not in run.step_results  # second step never ran

    @pytest.mark.asyncio
    async def test_step_exception_caught(self):
        """If a step executor raises, it's caught and marked FAILED."""
        engine = WorkflowEngine()
        wf = Workflow(name="exc", steps=[
            Step(id="bad", type=StepType.JOB, job_type="x"),
        ])

        # Don't patch JobRouter — import will fail or raise
        with patch("core.jobs.router.JobRouter", side_effect=RuntimeError("boom")):
            run = await engine.execute(wf)

        assert run.status == "failed"
        assert "boom" in run.error

    @pytest.mark.asyncio
    async def test_unknown_step_type(self):
        engine = WorkflowEngine()
        step = Step(id="x", type=StepType.JOB, job_type="noop")
        # Force an invalid type to test the default match branch
        step.type = "unknown_type"  # type: ignore
        wf = Workflow(name="unk", steps=[step])
        run = await engine.execute(wf)
        assert run.status == "failed"
        assert "Unknown type" in run.error

    @pytest.mark.asyncio
    async def test_condition_no_expr(self):
        engine = WorkflowEngine()
        wf = Workflow(name="c", steps=[
            Step(id="c1", type=StepType.CONDITION),
        ])
        run = await engine.execute(wf)
        assert run.status == "failed"
        assert "No expr" in run.error

    @pytest.mark.asyncio
    async def test_sub_workflow_not_found(self):
        engine = WorkflowEngine()
        wf = Workflow(name="sub", steps=[
            Step(id="s1", type=StepType.WORKFLOW, workflow_ref="nonexistent"),
        ])
        run = await engine.execute(wf)
        assert run.status == "failed"
        assert "not found" in run.error


# ── Timeouts ──

class TestWorkflowTimeouts:

    @pytest.mark.asyncio
    async def test_workflow_level_timeout(self):
        backend = FakeBackend(wait_delay=5)
        engine = WorkflowEngine()
        wf = Workflow(name="slow", steps=[_job_step("s1", "slow")], timeout_seconds=0.05)

        with _patch_job_router(backend):
            run = await engine.execute(wf)

        assert run.status == "failed"
        assert "timeout" in run.error.lower()

    @pytest.mark.asyncio
    async def test_job_step_timeout(self):
        backend = FakeBackend(wait_delay=5)
        engine = WorkflowEngine()
        step = _job_step("s1", "slow")
        step.timeout_seconds = 0.05
        wf = Workflow(name="t", steps=[step])

        with _patch_job_router(backend):
            run = await engine.execute(wf)

        assert run.status == "failed"
        assert "timed out" in run.error.lower()

    @pytest.mark.asyncio
    async def test_parallel_step_timeout(self):
        engine = WorkflowEngine()
        slow_branch = Step(id="b1", type=StepType.JOB, job_type="slow")
        fast_branch = Step(id="b2", type=StepType.JOB, job_type="fast")
        wf = Workflow(name="p", steps=[
            Step(id="par", type=StepType.PARALLEL, branches=[slow_branch, fast_branch],
                 timeout_seconds=0.05),
        ])

        backend = FakeBackend(wait_delay=5)
        with _patch_job_router(backend):
            run = await engine.execute(wf)

        assert run.status == "failed"
        assert "timed out" in run.error.lower()


# ── Retry ──

class TestWorkflowRetry:

    @pytest.mark.asyncio
    async def test_retry_then_succeed(self):
        call_count = 0

        class RetryBackend(FakeBackend):
            async def wait(self, job_id):
                nonlocal call_count
                call_count += 1
                if call_count < 3:
                    return FakeJobResult(status="failed", error="transient")
                return FakeJobResult(result={"ok": True})

        backend = RetryBackend()
        engine = WorkflowEngine()
        wf = Workflow(name="r", steps=[
            _job_step("s1", "flaky", retry=3),
        ])

        with _patch_job_router(backend), patch("asyncio.sleep", new_callable=AsyncMock):
            run = await engine.execute(wf)

        assert run.status == "completed"
        assert call_count == 3

    @pytest.mark.asyncio
    async def test_retry_exhausted(self):
        class AlwaysFailBackend(FakeBackend):
            async def wait(self, job_id):
                return FakeJobResult(status="failed", error="permanent")

        backend = AlwaysFailBackend()
        engine = WorkflowEngine()
        wf = Workflow(name="r", steps=[
            _job_step("s1", "bad", retry=2),
        ])

        with _patch_job_router(backend), patch("asyncio.sleep", new_callable=AsyncMock):
            run = await engine.execute(wf)

        assert run.status == "failed"
        assert "permanent" in run.error


# ── Condition / Jump ──

class TestWorkflowCondition:

    @pytest.mark.asyncio
    async def test_condition_then_branch(self):
        backend = FakeBackend(results={
            "job_0": FakeJobResult(result={"accuracy": 0.95}),
            "job_1": FakeJobResult(result={"deployed": True}),
        })
        engine = WorkflowEngine()
        wf = Workflow(name="c", steps=[
            _job_step("train", "train"),
            _cond_step("check", "steps.train.accuracy > 0.9", then_step="deploy", else_step="retrain"),
            _job_step("retrain", "retrain"),
            _job_step("deploy", "deploy"),
        ])

        with _patch_job_router(backend):
            run = await engine.execute(wf)

        assert run.status == "completed"
        assert run.step_results["retrain"].status == StepStatus.SKIPPED
        assert run.step_results["deploy"].status == StepStatus.COMPLETED

    @pytest.mark.asyncio
    async def test_condition_else_branch(self):
        backend = FakeBackend(results={
            "job_0": FakeJobResult(result={"accuracy": 0.5}),
            "job_1": FakeJobResult(result={"retrained": True}),
        })
        engine = WorkflowEngine()
        wf = Workflow(name="c", steps=[
            _job_step("train", "train"),
            _cond_step("check", "steps.train.accuracy > 0.9", then_step="deploy", else_step="retrain"),
            _job_step("retrain", "retrain"),
            _job_step("deploy", "deploy"),
        ])

        with _patch_job_router(backend):
            run = await engine.execute(wf)

        assert run.status == "completed"
        # else branch → retrain (no skip)
        assert run.step_results["retrain"].status == StepStatus.COMPLETED


# ── Loop ──

class TestWorkflowLoop:

    @pytest.mark.asyncio
    async def test_loop_until_condition(self):
        iteration = 0

        class IterBackend(FakeBackend):
            async def wait(self, job_id):
                nonlocal iteration
                iteration += 1
                return FakeJobResult(result={"score": 0.5 + iteration * 0.2})

        backend = IterBackend()
        engine = WorkflowEngine()
        wf = Workflow(name="l", steps=[
            Step(
                id="loop1", type=StepType.LOOP,
                body=[_job_step("improve", "improve")],
                until="steps.improve.score > 0.9",
                max_iterations=10,
            ),
        ])

        with _patch_job_router(backend):
            run = await engine.execute(wf)

        assert run.status == "completed"
        assert iteration <= 5  # should converge before 5

    @pytest.mark.asyncio
    async def test_loop_max_iterations_exceeded(self):
        backend = FakeBackend(results={"job_0": FakeJobResult(result={"score": 0.1})})
        engine = WorkflowEngine()
        wf = Workflow(name="l", steps=[
            Step(
                id="loop1", type=StepType.LOOP,
                body=[_job_step("s", "noop")],
                until="steps.s.score > 100",
                max_iterations=3,
            ),
        ])

        with _patch_job_router(backend):
            run = await engine.execute(wf)

        assert run.status == "failed"
        assert "max_iterations" in run.error


# ── Parallel ──

class TestWorkflowParallel:

    @pytest.mark.asyncio
    async def test_parallel_fan_out_fan_in(self):
        backend = FakeBackend(results={
            "job_0": FakeJobResult(result={"a": 1}),
            "job_1": FakeJobResult(result={"b": 2}),
        })
        engine = WorkflowEngine()
        wf = Workflow(name="p", steps=[
            Step(id="par", type=StepType.PARALLEL, branches=[
                _job_step("b1", "task_a"),
                _job_step("b2", "task_b"),
            ]),
        ])

        with _patch_job_router(backend):
            run = await engine.execute(wf)

        assert run.status == "completed"
        assert run.step_results["par"].output == {"b1": {"a": 1}, "b2": {"b": 2}}

    @pytest.mark.asyncio
    async def test_parallel_one_branch_fails(self):
        backend = FakeBackend(results={
            "job_0": FakeJobResult(result={"ok": True}),
            "job_1": FakeJobResult(status="failed", error="branch2 died"),
        })
        engine = WorkflowEngine()
        wf = Workflow(name="p", steps=[
            Step(id="par", type=StepType.PARALLEL, branches=[
                _job_step("b1", "ok"),
                _job_step("b2", "bad"),
            ]),
        ])

        with _patch_job_router(backend):
            run = await engine.execute(wf)

        assert run.status == "failed"
        assert "branch2 died" in run.error

    @pytest.mark.asyncio
    async def test_parallel_empty_branches(self):
        engine = WorkflowEngine()
        wf = Workflow(name="p", steps=[
            Step(id="par", type=StepType.PARALLEL, branches=[]),
        ])
        run = await engine.execute(wf)
        assert run.status == "completed"


# ── Cancel ──

class TestWorkflowCancel:

    @pytest.mark.asyncio
    async def test_cancel_during_execution(self):
        engine = WorkflowEngine()

        class SlowBackend(FakeBackend):
            async def wait(self, job_id):
                engine.cancel("cancel_wf")
                await asyncio.sleep(0)  # yield to let cancel propagate
                return FakeJobResult()

        backend = SlowBackend()
        wf = Workflow(name="cancel_wf", steps=[
            _job_step("s1", "slow"),
            _job_step("s2", "never"),
        ])

        with _patch_job_router(backend):
            run = await engine.execute(wf)

        assert run.status == "cancelled"
        assert "s2" not in run.step_results


# ── safe_eval ──

class TestSafeEval:

    def test_basic_comparisons(self):
        results = {"train": StepResult(step_id="train", status=StepStatus.COMPLETED, output={"acc": 0.95})}
        assert _safe_eval("steps.train.acc > 0.9", results) is True
        assert _safe_eval("steps.train.acc < 0.9", results) is False
        assert _safe_eval("steps.train.acc >= 0.95", results) is True
        assert _safe_eval("steps.train.acc == 0.95", results) is True
        assert _safe_eval("steps.train.acc != 1.0", results) is True

    def test_missing_step(self):
        assert _safe_eval("steps.missing.x > 0", {}) is False

    def test_invalid_expr(self):
        assert _safe_eval("not a valid expression", {}) is False


# ── Serialization round-trip ──

class TestWorkflowSerialization:

    def test_workflow_json_roundtrip(self):
        wf = Workflow(name="test", steps=[
            _job_step("s1", "train", retry=2, inputs={"lr": 0.01}),
            _wait_step("s2", "human:approve"),
            _cond_step("s3", "steps.s1.acc > 0.9", then_step="s4"),
            Step(id="s4", type=StepType.PARALLEL, branches=[
                _job_step("b1", "eval_a"),
                _job_step("b2", "eval_b"),
            ]),
        ])
        data = wf.model_dump()
        wf2 = Workflow(**data)
        assert wf2.name == wf.name
        assert len(wf2.steps) == 4
        assert wf2.steps[0].retry == 2

    def test_workflow_run_json_roundtrip(self):
        run = WorkflowRun(workflow_name="test")
        run.step_results["s1"] = StepResult(
            step_id="s1", status=StepStatus.COMPLETED, output={"x": 1},
        )
        data = run.model_dump()
        run2 = WorkflowRun(**data)
        assert run2.step_results["s1"].output == {"x": 1}


# ── Distributed cancel check ──

class TestDistributedCancelCheck:

    def test_is_cancelled_in_db_no_run_id(self):
        """No wf_run_id → always returns False."""
        engine = WorkflowEngine()
        assert engine._is_cancelled_in_db() is False

    def test_is_cancelled_in_db_with_run_id_not_cancelled(self):
        engine = WorkflowEngine(wf_run_id="wf-1")
        with patch("api.database.get_db_session") as mock_get:
            mock_db = MagicMock()
            mock_db.query.return_value.filter.return_value.first.return_value = None
            mock_get.return_value = iter([mock_db])
            assert engine._is_cancelled_in_db() is False

    def test_is_cancelled_in_db_with_run_id_cancelled(self):
        engine = WorkflowEngine(wf_run_id="wf-1")
        with patch("api.database.get_db_session") as mock_get:
            mock_db = MagicMock()
            mock_db.query.return_value.filter.return_value.first.return_value = MagicMock()
            mock_get.return_value = iter([mock_db])
            assert engine._is_cancelled_in_db() is True

    def test_is_cancelled_in_db_import_error(self):
        """DB unavailable → returns False (safe fallback)."""
        engine = WorkflowEngine(wf_run_id="wf-1")
        with patch("api.database.get_db_session", side_effect=Exception("no db")):
            assert engine._is_cancelled_in_db() is False
