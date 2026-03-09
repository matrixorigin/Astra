"""Unit tests for core/scheduling/workflow_engine.py."""

import pytest

from core.scheduling.workflow_engine import (
    WorkflowDefinition,
    WorkflowEngine,
    WorkflowExecution,
    WorkflowStep,
    WorkflowStatus,
    StepStatus,
)


def make_step(step_id, result=None, fail=False, depends_on=None):
    async def action(inp):
        if fail:
            raise ValueError(f"step {step_id} failed")
        return result or f"result_{step_id}"

    return WorkflowStep(
        step_id=step_id,
        name=f"Step {step_id}",
        action=action,
        depends_on=depends_on or [],
    )


def make_workflow(wf_id="wf-1", steps=None):
    wf = WorkflowDefinition(workflow_id=wf_id, name="Test WF", description="desc")
    for s in (steps or []):
        wf.add_step(s)
    return wf


class TestWorkflowDefinition:
    def test_add_step(self):
        wf = make_workflow(steps=[make_step("s1")])
        assert "s1" in wf.steps

    def test_to_dict(self):
        wf = make_workflow(steps=[make_step("s1")])
        d = wf.to_dict()
        assert d["workflow_id"] == "wf-1"
        assert "s1" in d["steps"]


class TestWorkflowStep:
    def test_to_dict(self):
        s = make_step("s1")
        d = s.to_dict()
        assert d["step_id"] == "s1"
        assert d["status"] == "pending"
        assert d["error"] is None


class TestWorkflowExecution:
    @pytest.mark.asyncio
    async def test_execute_single_step(self):
        wf = make_workflow(steps=[make_step("s1", result="done")])
        ex = WorkflowExecution(wf, context={})
        ok = await ex.execute()
        assert ok is True
        assert ex.status == WorkflowStatus.COMPLETED
        assert ex.step_results["s1"] == "done"

    @pytest.mark.asyncio
    async def test_execute_sequential_steps(self):
        s1 = make_step("s1")
        s2 = make_step("s2", depends_on=["s1"])
        wf = make_workflow(steps=[s1, s2])
        ex = WorkflowExecution(wf, context={})
        ok = await ex.execute()
        assert ok is True
        assert ex.step_results["s1"] is not None
        assert ex.step_results["s2"] is not None

    @pytest.mark.asyncio
    async def test_execute_step_failure(self):
        wf = make_workflow(steps=[make_step("s1", fail=True)])
        ex = WorkflowExecution(wf, context={})
        ok = await ex.execute()
        assert ok is False
        assert ex.status == WorkflowStatus.FAILED
        assert "s1" in ex.step_errors

    @pytest.mark.asyncio
    async def test_execute_circular_dependency(self):
        s1 = make_step("s1", depends_on=["s2"])
        s2 = make_step("s2", depends_on=["s1"])
        wf = make_workflow(steps=[s1, s2])
        ex = WorkflowExecution(wf, context={})
        ok = await ex.execute()
        assert ok is False
        assert ex.status == WorkflowStatus.FAILED

    @pytest.mark.asyncio
    async def test_execute_empty_workflow(self):
        wf = make_workflow(steps=[])
        ex = WorkflowExecution(wf, context={})
        ok = await ex.execute()
        assert ok is True
        assert ex.status == WorkflowStatus.COMPLETED

    def test_to_dict(self):
        wf = make_workflow(steps=[make_step("s1")])
        ex = WorkflowExecution(wf, context={})
        d = ex.to_dict()
        assert d["workflow_id"] == "wf-1"
        assert d["status"] == "draft"


class TestWorkflowEngine:
    @pytest.fixture
    def engine(self):
        return WorkflowEngine()

    def test_register_and_get(self, engine):
        wf = make_workflow()
        engine.register_workflow(wf)
        assert engine.get_workflow("wf-1") is wf

    def test_get_unknown_returns_none(self, engine):
        assert engine.get_workflow("nope") is None

    @pytest.mark.asyncio
    async def test_execute_workflow(self, engine):
        wf = make_workflow(steps=[make_step("s1")])
        engine.register_workflow(wf)
        ex = await engine.execute_workflow("wf-1", context={})
        assert ex is not None
        assert ex.status == WorkflowStatus.COMPLETED

    @pytest.mark.asyncio
    async def test_execute_unknown_workflow(self, engine):
        ex = await engine.execute_workflow("nope", context={})
        assert ex is None

    @pytest.mark.asyncio
    async def test_get_execution(self, engine):
        wf = make_workflow(steps=[make_step("s1")])
        engine.register_workflow(wf)
        ex = await engine.execute_workflow("wf-1", context={})
        assert engine.get_execution(ex.execution_id) is ex

    def test_get_execution_unknown(self, engine):
        assert engine.get_execution("nope") is None
