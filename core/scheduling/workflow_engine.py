"""Workflow engine for orchestrating multi-step processes.

Workflow definition, step execution, state management.
"""

from __future__ import annotations

import logging
from dataclasses import dataclass, field
from datetime import datetime
from enum import Enum
from typing import Any, Callable, Optional

logger = logging.getLogger(__name__)


class StepStatus(str, Enum):
    """Step execution status."""

    PENDING = "pending"
    RUNNING = "running"
    COMPLETED = "completed"
    FAILED = "failed"
    SKIPPED = "skipped"


class WorkflowStatus(str, Enum):
    """Workflow execution status."""

    DRAFT = "draft"
    RUNNING = "running"
    COMPLETED = "completed"
    FAILED = "failed"
    CANCELLED = "cancelled"


@dataclass
class WorkflowStep:
    """Single workflow step."""

    step_id: str
    name: str
    action: Callable
    depends_on: list[str] = field(default_factory=list)
    status: StepStatus = StepStatus.PENDING
    result: Any = None
    error: Optional[str] = None
    started_at: Optional[datetime] = None
    completed_at: Optional[datetime] = None

    def to_dict(self) -> dict:
        """Serialize to dict."""
        return {
            "step_id": self.step_id,
            "name": self.name,
            "status": self.status.value,
            "depends_on": self.depends_on,
            "started_at": self.started_at.isoformat() if self.started_at else None,
            "completed_at": self.completed_at.isoformat() if self.completed_at else None,
            "error": self.error,
        }


@dataclass
class WorkflowDefinition:
    """Workflow definition."""

    workflow_id: str
    name: str
    description: str
    steps: dict[str, WorkflowStep] = field(default_factory=dict)
    created_at: datetime = field(default_factory=datetime.now)

    def add_step(self, step: WorkflowStep) -> None:
        """Add step to workflow.

        Args:
            step: Workflow step
        """
        self.steps[step.step_id] = step

    def to_dict(self) -> dict:
        """Serialize to dict."""
        return {
            "workflow_id": self.workflow_id,
            "name": self.name,
            "description": self.description,
            "steps": {step_id: step.to_dict() for step_id, step in self.steps.items()},
            "created_at": self.created_at.isoformat(),
        }


class WorkflowExecution:
    """Execute workflow instance."""

    def __init__(self, workflow_def: WorkflowDefinition, context: dict):
        """Initialize workflow execution.

        Args:
            workflow_def: Workflow definition
            context: Execution context
        """
        self.workflow_def = workflow_def
        self.context = context
        self.execution_id = f"exec_{datetime.now().timestamp()}"
        self.status = WorkflowStatus.DRAFT
        self.started_at: Optional[datetime] = None
        self.completed_at: Optional[datetime] = None
        self.step_results: dict[str, Any] = {}
        self.step_errors: dict[str, str] = {}

    async def execute(self) -> bool:
        """Execute workflow.

        Returns:
            True if successful, False if failed
        """
        self.status = WorkflowStatus.RUNNING
        self.started_at = datetime.now()

        try:
            # Topological sort to respect dependencies
            executed = set()

            while len(executed) < len(self.workflow_def.steps):
                # Find next executable step
                next_step = None

                for step_id, step in self.workflow_def.steps.items():
                    if step_id in executed:
                        continue

                    # Check if all dependencies are met
                    if all(dep in executed for dep in step.depends_on):
                        next_step = step
                        break

                if not next_step:
                    # Circular dependency or no executable step
                    self.status = WorkflowStatus.FAILED
                    logger.error(f"Workflow {self.execution_id}: No executable step found")
                    return False

                # Execute step
                if not await self._execute_step(next_step):
                    self.status = WorkflowStatus.FAILED
                    self.completed_at = datetime.now()
                    return False

                executed.add(next_step.step_id)

            self.status = WorkflowStatus.COMPLETED
            self.completed_at = datetime.now()
            logger.info(f"Workflow completed: {self.execution_id}")
            return True

        except Exception as e:
            logger.error(f"Workflow error: {e}")
            self.status = WorkflowStatus.FAILED
            self.completed_at = datetime.now()
            return False

    async def _execute_step(self, step: WorkflowStep) -> bool:
        """Execute single step.

        Args:
            step: Step to execute

        Returns:
            True if successful
        """
        step.status = StepStatus.RUNNING
        step.started_at = datetime.now()

        try:
            # Prepare step input from context and previous results
            step_input = {
                "context": self.context,
                "results": self.step_results,
            }

            # Execute step action
            result = await step.action(step_input)

            step.status = StepStatus.COMPLETED
            step.result = result
            step.completed_at = datetime.now()
            self.step_results[step.step_id] = result

            logger.info(f"Step completed: {step.step_id}")
            return True

        except Exception as e:
            logger.error(f"Step failed: {step.step_id}, error: {e}")
            step.status = StepStatus.FAILED
            step.error = str(e)
            step.completed_at = datetime.now()
            self.step_errors[step.step_id] = str(e)
            return False

    def to_dict(self) -> dict:
        """Serialize to dict."""
        return {
            "execution_id": self.execution_id,
            "workflow_id": self.workflow_def.workflow_id,
            "status": self.status.value,
            "started_at": self.started_at.isoformat() if self.started_at else None,
            "completed_at": self.completed_at.isoformat() if self.completed_at else None,
            "step_results": self.step_results,
            "step_errors": self.step_errors,
        }


class WorkflowEngine:
    """Manage workflow definitions and executions."""

    def __init__(self):
        """Initialize engine."""
        self.workflows: dict[str, WorkflowDefinition] = {}
        self.executions: dict[str, WorkflowExecution] = {}

    def register_workflow(self, workflow: WorkflowDefinition) -> None:
        """Register workflow definition.

        Args:
            workflow: Workflow definition
        """
        self.workflows[workflow.workflow_id] = workflow
        logger.info(f"Registered workflow: {workflow.workflow_id}")

    def get_workflow(self, workflow_id: str) -> Optional[WorkflowDefinition]:
        """Get workflow definition.

        Args:
            workflow_id: Workflow ID

        Returns:
            Workflow or None
        """
        return self.workflows.get(workflow_id)

    async def execute_workflow(
        self,
        workflow_id: str,
        context: dict,
    ) -> Optional[WorkflowExecution]:
        """Execute workflow.

        Args:
            workflow_id: Workflow ID
            context: Execution context

        Returns:
            Execution or None if workflow not found
        """
        workflow = self.workflows.get(workflow_id)
        if not workflow:
            logger.error(f"Workflow not found: {workflow_id}")
            return None

        execution = WorkflowExecution(workflow, context)
        self.executions[execution.execution_id] = execution

        await execution.execute()
        return execution

    def get_execution(self, execution_id: str) -> Optional[WorkflowExecution]:
        """Get workflow execution.

        Args:
            execution_id: Execution ID

        Returns:
            Execution or None
        """
        return self.executions.get(execution_id)
