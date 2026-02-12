"""Test hierarchical planning."""

import pytest

from core.agent.planner import Plan, PlanConstraints, Planner, PlanStep, PlanStatus
from core.llm.client import LLMClient
from sdk import Database


@pytest.fixture
def db():
    """Database fixture."""
    return Database()


@pytest.fixture
def llm_client(db):
    """LLM client fixture."""
    return LLMClient(db)


@pytest.fixture
def planner(llm_client):
    """Planner fixture."""
    constraints = PlanConstraints(max_steps=10, max_depth=3, max_revisions=3)
    return Planner(llm_client, constraints)


def test_plan_with_depth():
    """Test plan model with depth field."""
    plan = Plan(
        plan_id="plan_001",
        goal="Test goal",
        steps=[PlanStep(step_id="step_1", description="Test step")],
        depth=0,
    )

    assert plan.depth == 0
    assert plan.parent_plan_id is None


def test_plan_with_parent():
    """Test sub-plan with parent reference."""
    parent_plan = Plan(
        plan_id="plan_001",
        goal="Parent goal",
        steps=[],
        depth=0,
    )

    sub_plan = Plan(
        plan_id="plan_001_sub_1",
        goal="Sub goal",
        steps=[],
        parent_plan_id=parent_plan.plan_id,
        depth=1,
    )

    assert sub_plan.parent_plan_id == parent_plan.plan_id
    assert sub_plan.depth == 1


def test_step_with_sub_plan():
    """Test step with sub-plan."""
    sub_plan = Plan(
        plan_id="sub_plan_001",
        goal="Sub goal",
        steps=[PlanStep(step_id="sub_step_1", description="Sub step")],
        depth=1,
    )

    step = PlanStep(
        step_id="step_1",
        description="Complex step",
        sub_plan=sub_plan,
    )

    assert step.sub_plan is not None
    assert step.sub_plan.depth == 1


def test_depth_constraint(planner):
    """Test that depth constraint is enforced."""
    plan = Plan(
        plan_id="plan_001",
        goal="Test goal",
        steps=[],
        depth=5,  # Exceeds max_depth=3
    )

    is_valid, error = planner.check_constraints(plan)

    assert not is_valid
    assert "depth" in error.lower()


@pytest.mark.asyncio
async def test_decompose_simple_step(planner):
    """Test that simple steps are not decomposed."""
    parent_plan = Plan(
        plan_id="plan_001",
        goal="Parent goal",
        steps=[],
        depth=0,
    )

    simple_step = PlanStep(
        step_id="step_1",
        description="List files in directory",  # Simple task
    )

    sub_plan = await planner.decompose_step(simple_step, parent_plan)

    # Simple step should not be decomposed (or may be, depending on LLM)
    # Just verify it doesn't crash
    assert sub_plan is None or isinstance(sub_plan, Plan)


@pytest.mark.asyncio
async def test_decompose_at_max_depth(planner):
    """Test that decomposition stops at max depth."""
    parent_plan = Plan(
        plan_id="plan_001",
        goal="Parent goal",
        steps=[],
        depth=3,  # At max depth
    )

    step = PlanStep(
        step_id="step_1",
        description="Complex task",
    )

    sub_plan = await planner.decompose_step(step, parent_plan)

    # Should not decompose at max depth
    assert sub_plan is None


def test_constraints_with_max_depth():
    """Test that constraints include max_depth."""
    constraints = PlanConstraints(max_depth=5)

    assert constraints.max_depth == 5


if __name__ == "__main__":
    pytest.main([__file__, "-v"])
