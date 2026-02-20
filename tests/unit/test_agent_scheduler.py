"""Tests for agent scheduling and resource management."""

from unittest.mock import Mock

import pytest

from core.agents.scheduler import AgentScheduler, BudgetPolicy, Priority, ResourceAllocation


def _mock_db():
    return Mock()


class TestPriority:
    def test_priority_values(self):
        assert Priority.P0.value == 0
        assert Priority.P1.value == 1
        assert Priority.P2.value == 2
        assert Priority.P3.value == 3

    def test_priority_ordering(self):
        assert Priority.P0 < Priority.P1 < Priority.P2 < Priority.P3


class TestBudgetPolicy:
    def test_burn_rate_target(self):
        policy = BudgetPolicy(
            scope_id="user-1",
            daily_budget=100.0,
            current_spend=40.0,
            remaining_hours=8.0,
        )
        # (100 - 40) / 8 = 7.5 $/hour
        assert policy.burn_rate_target == 7.5

    def test_burn_rate_target_zero_hours(self):
        policy = BudgetPolicy(
            scope_id="user-1",
            daily_budget=100.0,
            current_spend=40.0,
            remaining_hours=0.0,
        )
        # Should not divide by zero
        assert policy.burn_rate_target == 60.0  # remaining / max(0, 1)

    def test_should_downgrade_model_high_burn(self):
        policy = BudgetPolicy(
            scope_id="user-1",
            daily_budget=100.0,
            current_spend=90.0,
            remaining_hours=1.0,
        )
        # current_burn_rate = 90 / (24 - 1) = 4.1
        # burn_rate_target = 10 / 1 = 10
        # 4.1 < 10 → no downgrade (we're actually under budget)
        assert policy.should_downgrade_model(6.0) is False

    def test_should_downgrade_model_very_high_burn(self):
        policy = BudgetPolicy(
            scope_id="user-1",
            daily_budget=100.0,
            current_spend=90.0,
            remaining_hours=23.0,  # Only 1 hour left
        )
        # current_burn_rate = 90 / (24 - 23) = 90
        # burn_rate_target = 10 / 23 = 0.43
        # 90 > 0.43 → downgrade
        assert policy.should_downgrade_model(6.0) is True


class TestResourceAllocation:
    def test_allocation_success(self):
        alloc = ResourceAllocation(
            task_id="t1",
            priority=Priority.P0,
            estimated_cost=5.0,
            allocated=True,
        )
        assert alloc.allocated is True
        assert alloc.reason is None

    def test_allocation_failure(self):
        alloc = ResourceAllocation(
            task_id="t1",
            priority=Priority.P2,
            estimated_cost=50.0,
            allocated=False,
            reason="budget_exceeded",
        )
        assert alloc.allocated is False
        assert alloc.reason == "budget_exceeded"


class TestAgentScheduler:
    def test_submit_task_success(self):
        db = _mock_db()
        db.execute.side_effect = [
            Mock(fetchone=Mock(return_value=(100.0, 10.0))),  # Budget policy
            None,  # Record allocation
        ]

        scheduler = AgentScheduler(db)
        alloc = scheduler.submit_task(
            task_id="t1",
            agent_id="agent-1",
            priority=Priority.P0,
            estimated_cost=5.0,
            scope_id="user-1",
        )

        assert alloc.allocated is True
        db.execute.assert_called()
        db.commit.assert_called()

    def test_submit_task_budget_exceeded(self):
        db = _mock_db()
        db.execute.return_value = Mock(
            fetchone=Mock(return_value=(100.0, 95.0))  # Only $5 left
        )

        scheduler = AgentScheduler(db)
        alloc = scheduler.submit_task(
            task_id="t1",
            agent_id="agent-1",
            priority=Priority.P1,
            estimated_cost=10.0,  # Exceeds remaining
            scope_id="user-1",
        )

        assert alloc.allocated is False
        assert alloc.reason == "budget_exceeded"

    def test_submit_task_resource_exhausted(self):
        db = _mock_db()
        db.execute.return_value = Mock(
            fetchone=Mock(return_value=(100.0, 10.0))  # Budget OK
        )

        scheduler = AgentScheduler(db)
        # Mock _check_resource_pools to return False
        scheduler._check_resource_pools = Mock(return_value=False)

        alloc = scheduler.submit_task(
            task_id="t1",
            agent_id="agent-1",
            priority=Priority.P2,
            estimated_cost=5.0,
            scope_id="user-1",
        )

        assert alloc.allocated is False
        assert alloc.reason == "resource_exhausted"

    def test_get_model_recommendation_high_burn(self):
        db = _mock_db()
        db.execute.return_value = Mock(
            fetchone=Mock(return_value=(100.0, 95.0))  # High burn rate
        )

        scheduler = AgentScheduler(db)
        model = scheduler.get_model_recommendation(
            scope_id="user-1",
            task_type="code_review",
            available_models=["gpt-4", "gpt-3.5"],
        )

        # Should recommend cheaper model
        assert model == "gpt-3.5"

    def test_get_model_recommendation_low_burn(self):
        db = _mock_db()
        db.execute.return_value = Mock(
            fetchone=Mock(return_value=(100.0, 10.0))  # Low burn rate
        )

        scheduler = AgentScheduler(db)
        model = scheduler.get_model_recommendation(
            scope_id="user-1",
            task_type="code_review",
            available_models=["gpt-4", "gpt-3.5"],
        )

        # Should recommend best model
        assert model == "gpt-4"

    def test_should_shed_load_p0(self):
        db = _mock_db()
        scheduler = AgentScheduler(db)
        assert scheduler.should_shed_load(Priority.P0) is False

    def test_should_shed_load_p3(self):
        db = _mock_db()
        scheduler = AgentScheduler(db)
        assert scheduler.should_shed_load(Priority.P3) is True

    def test_should_shed_load_p1_p2(self):
        db = _mock_db()
        scheduler = AgentScheduler(db)
        assert scheduler.should_shed_load(Priority.P1) is False
        assert scheduler.should_shed_load(Priority.P2) is False

    def test_get_budget_policy_from_db(self):
        db = _mock_db()
        db.execute.return_value = Mock(
            fetchone=Mock(return_value=(100.0, 25.0))
        )

        scheduler = AgentScheduler(db)
        policy = scheduler._get_budget_policy("user-1")

        assert policy.daily_budget == 100.0
        assert policy.current_spend == 25.0
        assert policy.remaining_hours > 0

    def test_get_budget_policy_default(self):
        db = _mock_db()
        db.execute.return_value = Mock(fetchone=Mock(return_value=None))

        scheduler = AgentScheduler(db)
        policy = scheduler._get_budget_policy("user-1")

        assert policy.daily_budget == 100.0
        assert policy.current_spend == 0.0
