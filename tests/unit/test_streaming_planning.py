"""Unit tests for streaming and planning functionality."""

import unittest
from unittest.mock import MagicMock

from core.agent.chat_loop import _merge_tool_call_fragments
from core.agent.planner import Plan, Planner, PlanStatus, PlanStep


class TestMergeToolCallFragments(unittest.TestCase):
    """Test _merge_tool_call_fragments function."""

    def test_merge_single_fragment(self):
        """Test merging a single fragment."""
        fragments = []
        new_fragments = [
            {"id": "call_1", "function": {"name": "test", "arguments": '{"key": "value"}'}}
        ]

        result = _merge_tool_call_fragments(fragments, new_fragments)

        self.assertEqual(len(result), 1)
        self.assertEqual(result[0]["id"], "call_1")
        self.assertEqual(result[0]["function"]["name"], "test")

    def test_merge_multiple_fragments(self):
        """Test merging multiple fragments for same call."""
        fragments = [{"id": "call_1", "function": {"name": "test", "arguments": '{"key": "val'}}]
        new_fragments = [{"id": "call_1", "function": {"name": "test", "arguments": 'ue"}'}}]

        result = _merge_tool_call_fragments(fragments, new_fragments)

        self.assertEqual(len(result), 1)
        self.assertEqual(result[0]["function"]["arguments"], '{"key": "value"}')

    def test_merge_different_calls(self):
        """Test merging fragments for different tool calls."""
        fragments = [{"id": "call_1", "function": {"name": "tool1", "arguments": '{"a": 1}'}}]
        new_fragments = [{"id": "call_2", "function": {"name": "tool2", "arguments": '{"b": 2}'}}]

        result = _merge_tool_call_fragments(fragments, new_fragments)

        self.assertEqual(len(result), 2)
        names = [r["function"]["name"] for r in result]
        self.assertIn("tool1", names)
        self.assertIn("tool2", names)


class TestPlannerGetNextSteps(unittest.TestCase):
    """Test Planner.get_next_steps method."""

    def setUp(self):
        self.llm_client = MagicMock()
        self.planner = Planner(self.llm_client)

    def test_get_next_steps_all_pending(self):
        """Test getting next steps when all are pending."""
        plan = Plan(
            plan_id="plan_1",
            goal="Test",
            steps=[
                PlanStep(step_id="step_1", description="Step 1"),
                PlanStep(step_id="step_2", description="Step 2"),
            ],
        )

        next_steps = self.planner.get_next_steps(plan)

        self.assertEqual(len(next_steps), 2)

    def test_get_next_steps_with_completed(self):
        """Test getting next steps with some completed."""
        plan = Plan(
            plan_id="plan_1",
            goal="Test",
            steps=[
                PlanStep(step_id="step_1", description="Step 1", status=PlanStatus.COMPLETED),
                PlanStep(step_id="step_2", description="Step 2", status=PlanStatus.PENDING),
            ],
        )

        next_steps = self.planner.get_next_steps(plan)

        self.assertEqual(len(next_steps), 1)
        self.assertEqual(next_steps[0].step_id, "step_2")


class TestPlannerCheckConstraints(unittest.TestCase):
    """Test Planner.check_constraints method."""

    def setUp(self):
        self.llm_client = MagicMock()
        self.planner = Planner(self.llm_client)

    def test_constraints_valid(self):
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

    def test_constraints_exceeded(self):
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


if __name__ == "__main__":
    unittest.main()
