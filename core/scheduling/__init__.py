"""Auto-scheduling system — P4 System Agent Auto-Scheduling.

Event-driven triggers + task scheduler + workflow engine.
"""

from core.scheduling.task_scheduler import Task, TaskScheduler, TaskStatus
from core.scheduling.trigger_rules import (
    Condition,
    ConditionLogic,
    ConditionOperator,
    TriggerRule,
    TriggerRuleRegistry,
)
from core.scheduling.workflow_engine import (
    StepStatus,
    WorkflowDefinition,
    WorkflowEngine,
    WorkflowExecution,
    WorkflowStatus,
    WorkflowStep,
)

__all__ = [
    # Trigger rules
    "Condition",
    "ConditionOperator",
    "ConditionLogic",
    "TriggerRule",
    "TriggerRuleRegistry",
    # Task scheduler
    "Task",
    "TaskStatus",
    "TaskScheduler",
    # Workflow engine
    "WorkflowStep",
    "WorkflowDefinition",
    "WorkflowExecution",
    "WorkflowStatus",
    "StepStatus",
    "WorkflowEngine",
]
