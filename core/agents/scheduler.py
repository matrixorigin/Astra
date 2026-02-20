"""Agent scheduling and resource management.

Design ref: agents-and-orchestration.md §9 "Agent Scheduling and Resource Management"

Manages priority queues, resource pools, and budget convergence.
Prevents thundering herds, budget overruns, and priority inversions.

Distributed-safe: all state in DB, no in-memory queues.
"""

from __future__ import annotations

import logging
from dataclasses import dataclass
from datetime import datetime, timedelta, timezone
from enum import Enum
from typing import Any

from sqlalchemy import and_, text
from sqlalchemy.orm import Session

logger = logging.getLogger(__name__)


class Priority(int, Enum):
    """Task priority levels."""

    P0 = 0  # User-facing interactive (< 2s latency)
    P1 = 1  # User-initiated background (< 30s)
    P2 = 2  # System-initiated (evaluation, training)
    P3 = 3  # Speculative (exploration, pre-warming)


@dataclass
class BudgetPolicy:
    """Budget convergence policy."""

    scope_id: str
    daily_budget: float
    current_spend: float
    remaining_hours: float

    @property
    def burn_rate_target(self) -> float:
        """Target $/hour to stay within budget."""
        remaining = self.daily_budget - self.current_spend
        return remaining / max(self.remaining_hours, 1)

    def should_downgrade_model(self, estimated_cost: float) -> bool:
        """Switch to cheaper model if burn rate exceeds target."""
        # Only downgrade if current burn rate is already high
        # (i.e., we're spending faster than the target rate)
        current_burn_rate = self.current_spend / max(24 - self.remaining_hours, 1)
        return current_burn_rate > self.burn_rate_target


@dataclass
class ResourceAllocation:
    """Resource allocation for a task."""

    task_id: str
    priority: Priority
    estimated_cost: float
    allocated: bool
    reason: str | None = None


class AgentScheduler:
    """Schedule agent tasks with resource management.

    Distributed-safe: all state in DB, no in-memory queues.
    """

    def __init__(self, db: Session) -> None:
        self.db = db

    def submit_task(
        self,
        task_id: str,
        agent_id: str,
        priority: Priority,
        estimated_cost: float,
        scope_id: str,
    ) -> ResourceAllocation:
        """Submit a task for scheduling.

        Args:
            task_id: Task identifier
            agent_id: Agent running the task
            priority: Priority level
            estimated_cost: Estimated cost in dollars
            scope_id: Scope (user, team, account)

        Returns:
            ResourceAllocation with allocated=True/False
        """
        # Check budget
        policy = self._get_budget_policy(scope_id)
        if policy.current_spend + estimated_cost > policy.daily_budget:
            logger.warning(
                f"Task {task_id} rejected: budget exceeded "
                f"({policy.current_spend} + {estimated_cost} > {policy.daily_budget})"
            )
            return ResourceAllocation(
                task_id=task_id,
                priority=priority,
                estimated_cost=estimated_cost,
                allocated=False,
                reason="budget_exceeded",
            )

        # Check resource pools
        if not self._check_resource_pools(priority):
            logger.warning(f"Task {task_id} rejected: resource pool exhausted")
            return ResourceAllocation(
                task_id=task_id,
                priority=priority,
                estimated_cost=estimated_cost,
                allocated=False,
                reason="resource_exhausted",
            )

        # Allocate
        self._record_allocation(task_id, agent_id, priority, estimated_cost, scope_id)
        logger.info(f"Task {task_id} allocated (priority={priority.name}, cost=${estimated_cost})")

        return ResourceAllocation(
            task_id=task_id,
            priority=priority,
            estimated_cost=estimated_cost,
            allocated=True,
        )

    def get_model_recommendation(
        self,
        scope_id: str,
        task_type: str,
        available_models: list[str],
    ) -> str:
        """Get recommended model based on budget convergence.

        Args:
            scope_id: Scope
            task_type: Type of task
            available_models: List of available models (assumed in cost order: expensive first)

        Returns:
            Recommended model ID
        """
        policy = self._get_budget_policy(scope_id)

        # If burn rate is high, downgrade to cheaper model
        if policy.should_downgrade_model(10.0):  # Assume $10 for expensive model
            if len(available_models) > 1:
                logger.info(f"Downgrading model for {task_type} due to burn rate")
                return available_models[-1]  # Cheapest

        return available_models[0]  # Default (most capable)

    def should_shed_load(self, priority: Priority) -> bool:
        """Decide whether to shed a task.

        Args:
            priority: Task priority

        Returns:
            True if task should be shed
        """
        if priority == Priority.P0:
            return False  # Never shed interactive
        if priority == Priority.P3:
            return True  # Always shed speculative if needed
        # P1, P2: queue instead of shed
        return False

    def _get_budget_policy(self, scope_id: str) -> BudgetPolicy:
        """Get budget policy for a scope."""
        # Query DB for budget info
        row = self.db.execute(
            text(
                "SELECT daily_budget, current_spend FROM budget_policies "
                "WHERE scope_id = :scope_id"
            ),
            {"scope_id": scope_id},
        ).fetchone()

        if row:
            daily_budget, current_spend = row
        else:
            # Default: $100/day
            daily_budget = 100.0
            current_spend = 0.0

        # Calculate remaining hours until midnight
        now = datetime.now(timezone.utc)
        midnight = (now + timedelta(days=1)).replace(hour=0, minute=0, second=0, microsecond=0)
        remaining_hours = (midnight - now).total_seconds() / 3600

        return BudgetPolicy(
            scope_id=scope_id,
            daily_budget=daily_budget,
            current_spend=current_spend,
            remaining_hours=remaining_hours,
        )

    def _check_resource_pools(self, priority: Priority) -> bool:
        """Check if resource pools have capacity.

        Queries active (non-completed) task allocations in the last hour.
        Each priority level has a concurrency limit.
        """
        limits = {Priority.P0: 50, Priority.P1: 30, Priority.P2: 20, Priority.P3: 10}
        limit = limits.get(priority, 10)

        row = self.db.execute(
            text(
                "SELECT COUNT(*) FROM task_allocations "
                "WHERE priority = :priority "
                "AND allocated_at > DATE_SUB(NOW(), INTERVAL 1 HOUR) "
                "AND completed_at IS NULL"
            ),
            {"priority": priority.value},
        ).fetchone()

        active = row[0] if row else 0
        if active >= limit:
            logger.warning(f"Resource pool exhausted for {priority.name}: {active}/{limit}")
            return False
        return True

    def _record_allocation(
        self,
        task_id: str,
        agent_id: str,
        priority: Priority,
        estimated_cost: float,
        scope_id: str,
    ) -> None:
        """Record task allocation in DB."""
        from uuid_utils import uuid7

        self.db.execute(
            text(
                "INSERT INTO task_allocations "
                "(allocation_id, task_id, agent_id, priority, estimated_cost, scope_id, allocated_at) "
                "VALUES (:id, :task_id, :agent_id, :priority, :cost, :scope_id, NOW())"
            ),
            {
                "id": str(uuid7()),
                "task_id": task_id,
                "agent_id": agent_id,
                "priority": priority.value,
                "cost": estimated_cost,
                "scope_id": scope_id,
            },
        )
        self.db.commit()
