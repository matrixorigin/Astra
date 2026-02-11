"""Unit tests for autonomous planning functionality."""

import unittest
from unittest.mock import MagicMock, AsyncMock
from core.agent.planner import (
    Planner,
    Plan,
    PlanStep,
    PlanStatus,
    PlanConstraints,
    get_plan_constraints,
)


class TestPlanConstraints(unittest.TestCase):
    """Test PlanConstraints model."""

    def test_default_constraints(self):
        """Test default constraint values."""
        constraints = PlanConstraints()

        self.assertEqual(constraints.max_steps, 10)
        self.assertEqual(constraints.max_revisions, 3)
        self.assertEqual(constraints.timeout_seconds, 300)
        self.assertIsNone(constraints.cost_budget_usd)
        self.assertFalse(constraints.sandbox_required)

    def test_custom_constraints(self):
        """Test custom constraint values."""
        constraints = PlanConstraints(
            max_steps=20,
            max_revisions=5,
            timeout_seconds=600,
            cost_budget_usd=10.0,
            sandbox_required=True,
        )

        self.assertEqual(constraints.max_steps, 20)
        self.assertEqual(constraints.max_revisions, 5)
        self.assertEqual(constraints.timeout_seconds, 600)
        self.assertEqual(constraints.cost_budget_usd, 10.0)
        self.assertTrue(constraints.sandbox_required)


class TestPlanStep(unittest.TestCase):
    """Test PlanStep model."""

    def test_plan_step_creation(self):
        """Test creating a PlanStep."""
        step = PlanStep(
            step_id="step_1",
            description="Do something",
            skill_hint="test_skill",
            depends_on=[],
            status=PlanStatus.PENDING,
        )

        self.assertEqual(step.step_id, "step_1")
        self.assertEqual(step.description, "Do something")
        self.assertEqual(step.skill_hint, "test_skill")
        self.assertEqual(step.status, PlanStatus.PENDING)

    def test_plan_step_with_dependencies(self):
        """Test PlanStep with dependencies."""
        step = PlanStep(
            step_id="step_2",
            description="Do something else",
            depends_on=["step_1"],
        )

        self.assertEqual(step.depends_on, ["step_1"])


class TestPlan(unittest.TestCase):
    """Test Plan model."""

    def test_plan_creation(self):
        """Test creating a Plan."""
        plan = Plan(
            plan_id="plan_1",
            goal="Complete a task",
            steps=[
                PlanStep(step_id="step_1", description="Step 1"),
                PlanStep(step_id="step_2", description="Step 2"),
            ],
        )

        self.assertEqual(plan.plan_id, "plan_1")
        self.assertEqual(plan.goal, "Complete a task")
        self.assertEqual(len(plan.steps), 2)

    def test_plan_with_revision(self):
        """Test Plan with revision_of."""
        plan = Plan(
            plan_id="plan_1",
            goal="Complete a task",
            steps=[],
            revision_of="plan_0",
        )

        self.assertEqual(plan.revision_of, "plan_0")


class TestPlanner(unittest.TestCase):
    """Test Planner class."""

    def setUp(self):
        self.llm_client = MagicMock()
        self.planner = Planner(self.llm_client)

    def test_get_plan_constraints(self):
        """Test get_plan_constraints function."""
        constraints = get_plan_constraints()

        self.assertIsInstance(constraints, PlanConstraints)
        self.assertEqual(constraints.max_steps, 10)

    def test_check_constraints_valid(self):
        """Test check_constraints with valid plan."""
        plan = Plan(
            plan_id="plan_1",
            goal="Test",
            steps=[
                PlanStep(step_id="step_1", description="Step 1"),
                PlanStep(step_id="step_2", description="Step 2"),
            ],
        )

        is_valid, error = self.planner.check_constraints(plan)

        self.assertTrue(is_valid)
        self.assertIsNone(error)

    def test_check_constraints_exceeded(self):
        """Test check_constraints with too many steps."""
        plan = Plan(
            plan_id="plan_1",
            goal="Test",
            steps=[PlanStep(step_id=f"step_{i}", description=f"Step {i}") for i in range(15)],
        )

        is_valid, error = self.planner.check_constraints(plan)

        self.assertFalse(is_valid)
        self.assertIn("15 steps", error)
        self.assertIn("max is 10", error)

    def test_get_next_steps(self):
        """Test get_next_steps with pending steps."""
        plan = Plan(
            plan_id="plan_1",
            goal="Test",
            steps=[
                PlanStep(step_id="step_1", description="Step 1", status=PlanStatus.COMPLETED),
                PlanStep(step_id="step_2", description="Step 2", status=PlanStatus.PENDING),
                PlanStep(step_id="step_3", description="Step 3", status=PlanStatus.PENDING),
            ],
        )

        next_steps = self.planner.get_next_steps(plan)

        self.assertEqual(len(next_steps), 2)
        step_ids = [s.step_id for s in next_steps]
        self.assertIn("step_2", step_ids)
        self.assertIn("step_3", step_ids)

    def test_get_next_steps_with_dependencies(self):
        """Test get_next_steps with step dependencies."""
        plan = Plan(
            plan_id="plan_1",
            goal="Test",
            steps=[
                PlanStep(step_id="step_1", description="Step 1", status=PlanStatus.COMPLETED),
                PlanStep(
                    step_id="step_2",
                    description="Step 2",
                    depends_on=["step_1"],
                    status=PlanStatus.PENDING,
                ),
                PlanStep(
                    step_id="step_3",
                    description="Step 3",
                    depends_on=["step_2"],
                    status=PlanStatus.PENDING,
                ),
            ],
        )

        next_steps = self.planner.get_next_steps(plan)

        # Only step_2 should be next (step_3 depends on step_2)
        self.assertEqual(len(next_steps), 1)
        self.assertEqual(next_steps[0].step_id, "step_2")


if __name__ == "__main__":
    unittest.main()
