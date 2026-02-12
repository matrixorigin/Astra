"""Tests for plan sandbox integration."""

from unittest.mock import MagicMock, patch

import pytest

from core.agent.planner import Plan, PlanStep, execute_plan_in_sandbox


@pytest.fixture
def mock_db():
    """Mock database."""
    db = MagicMock()
    return db


@pytest.fixture
def sample_plan():
    """Sample plan for testing."""
    return Plan(
        plan_id="plan_test_001",
        goal="Test deployment",
        steps=[
            PlanStep(step_id="step_1", description="Run tests"),
            PlanStep(step_id="step_2", description="Build image"),
            PlanStep(step_id="step_3", description="Deploy"),
        ],
    )


class TestExecutePlanInSandbox:
    """Test plan execution in sandbox."""

    @patch("core.sandbox.sandbox.Sandbox")
    def test_execute_plan_success(self, mock_sandbox_class, mock_db, sample_plan):
        """Test successful plan execution in sandbox."""
        # Mock sandbox
        mock_sandbox = MagicMock()
        mock_sandbox_class.return_value = mock_sandbox

        # Mock executor
        def mock_executor(step):
            return {"output": f"Executed {step.step_id}"}

        # Execute
        result = execute_plan_in_sandbox(
            plan=sample_plan,
            db=mock_db,
            executor_fn=mock_executor,
            sandbox_name="test_sandbox",
        )

        # Verify sandbox created
        mock_sandbox.create.assert_called_once()
        assert "test_sandbox" in str(mock_sandbox.create.call_args)

        # Verify all steps executed
        assert result["success"] is True
        assert len(result["steps"]) == 3
        assert result["steps"][0]["step_id"] == "step_1"
        assert result["steps"][0]["success"] is True

        # Verify sandbox deleted
        mock_sandbox.delete.assert_called_once_with("test_sandbox")

    @patch("core.sandbox.sandbox.Sandbox")
    def test_execute_plan_step_failure(self, mock_sandbox_class, mock_db, sample_plan):
        """Test plan execution with step failure."""
        mock_sandbox = MagicMock()
        mock_sandbox_class.return_value = mock_sandbox

        # Mock executor that fails on step 2
        def mock_executor(step):
            if step.step_id == "step_2":
                raise Exception("Build failed")
            return {"output": f"Executed {step.step_id}"}

        # Execute
        result = execute_plan_in_sandbox(
            plan=sample_plan,
            db=mock_db,
            executor_fn=mock_executor,
        )

        # Verify failure
        assert result["success"] is False
        assert len(result["steps"]) == 2  # Stopped after step 2
        assert result["steps"][0]["success"] is True
        assert result["steps"][1]["success"] is False
        assert "Build failed" in result["steps"][1]["error"]

        # Verify sandbox cleaned up
        mock_sandbox.delete.assert_called_once()

    @patch("core.sandbox.sandbox.Sandbox")
    def test_execute_plan_sandbox_creation_failure(self, mock_sandbox_class, mock_db, sample_plan):
        """Test handling of sandbox creation failure."""
        mock_sandbox = MagicMock()
        mock_sandbox.create.side_effect = Exception("Sandbox creation failed")
        mock_sandbox_class.return_value = mock_sandbox

        def mock_executor(step):
            return {"output": "ok"}

        # Execute
        result = execute_plan_in_sandbox(
            plan=sample_plan,
            db=mock_db,
            executor_fn=mock_executor,
        )

        # Verify failure
        assert result["success"] is False
        assert "Sandbox creation failed" in result["error"]
        assert len(result["steps"]) == 0

    @patch("core.sandbox.sandbox.Sandbox")
    def test_execute_plan_auto_generated_name(self, mock_sandbox_class, mock_db, sample_plan):
        """Test auto-generated sandbox name."""
        mock_sandbox = MagicMock()
        mock_sandbox_class.return_value = mock_sandbox

        def mock_executor(step):
            return {"output": "ok"}

        # Execute without sandbox_name
        result = execute_plan_in_sandbox(
            plan=sample_plan,
            db=mock_db,
            executor_fn=mock_executor,
        )

        # Verify auto-generated name
        assert result["sandbox_name"].startswith("plan_dry_run_")
        assert len(result["sandbox_name"]) > len("plan_dry_run_")

        # Verify sandbox created with auto-generated name
        mock_sandbox.create.assert_called_once()
        call_args = mock_sandbox.create.call_args
        assert call_args[1]["name"].startswith("plan_dry_run_")

    @patch("core.sandbox.sandbox.Sandbox")
    def test_execute_plan_metadata(self, mock_sandbox_class, mock_db, sample_plan):
        """Test sandbox metadata includes plan info."""
        mock_sandbox = MagicMock()
        mock_sandbox_class.return_value = mock_sandbox

        def mock_executor(step):
            return {"output": "ok"}

        # Execute
        result = execute_plan_in_sandbox(
            plan=sample_plan,
            db=mock_db,
            executor_fn=mock_executor,
            sandbox_name="test_sandbox",
        )

        # Verify metadata
        call_args = mock_sandbox.create.call_args
        assert "plan_test_001" in call_args[1]["description"]
        assert "Test deployment" in call_args[1]["description"]
        assert "dry-run" in call_args[1]["tags"]
        assert "plan" in call_args[1]["tags"]
        assert "plan_test_001" in call_args[1]["tags"]
        assert call_args[1]["created_by"] == "planner"

    @patch("core.sandbox.sandbox.Sandbox")
    def test_execute_plan_cleanup_on_error(self, mock_sandbox_class, mock_db, sample_plan):
        """Test sandbox cleanup even when deletion fails."""
        mock_sandbox = MagicMock()
        mock_sandbox.delete.side_effect = Exception("Delete failed")
        mock_sandbox_class.return_value = mock_sandbox

        def mock_executor(step):
            raise Exception("Step failed")

        # Execute - should not raise despite delete failure
        result = execute_plan_in_sandbox(
            plan=sample_plan,
            db=mock_db,
            executor_fn=mock_executor,
        )

        # Verify execution failed but didn't crash
        assert result["success"] is False
        # Delete called at least once (may be called twice due to cleanup logic)
        assert mock_sandbox.delete.call_count >= 1
