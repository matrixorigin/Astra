"""Unit tests for core/scheduling/trigger_rules.py and task_scheduler.py."""

import asyncio
import pytest

from core.scheduling.trigger_rules import (
    Condition, ConditionOperator, ConditionLogic,
    TriggerRule, TriggerRuleRegistry,
)


def make_rule(rule_id="r1", event_type="user_query", conditions=None, logic=ConditionLogic.AND, enabled=True):
    return TriggerRule(
        rule_id=rule_id, name=f"Rule {rule_id}", description="test",
        event_type=event_type, conditions=conditions or [], logic=logic, enabled=enabled,
    )


class TestCondition:
    def _event(self, **data):
        return {"event_type": "user_query", "data": data}

    def test_eq(self):
        c = Condition(field="data.status", operator=ConditionOperator.EQ, value="active")
        assert c.matches(self._event(status="active")) is True
        assert c.matches(self._event(status="inactive")) is False

    def test_ne(self):
        c = Condition(field="data.status", operator=ConditionOperator.NE, value="active")
        assert c.matches(self._event(status="inactive")) is True

    def test_gt_lt(self):
        c_gt = Condition(field="data.score", operator=ConditionOperator.GT, value=5)
        c_lt = Condition(field="data.score", operator=ConditionOperator.LT, value=5)
        assert c_gt.matches(self._event(score=6)) is True
        assert c_gt.matches(self._event(score=4)) is False
        assert c_lt.matches(self._event(score=4)) is True

    def test_gte_lte(self):
        c_gte = Condition(field="data.score", operator=ConditionOperator.GTE, value=5)
        c_lte = Condition(field="data.score", operator=ConditionOperator.LTE, value=5)
        assert c_gte.matches(self._event(score=5)) is True
        assert c_lte.matches(self._event(score=5)) is True

    def test_in_not_in(self):
        c_in = Condition(field="data.type", operator=ConditionOperator.IN, value=["a", "b"])
        c_nin = Condition(field="data.type", operator=ConditionOperator.NOT_IN, value=["a", "b"])
        assert c_in.matches(self._event(type="a")) is True
        assert c_in.matches(self._event(type="c")) is False
        assert c_nin.matches(self._event(type="c")) is True

    def test_contains(self):
        c = Condition(field="data.msg", operator=ConditionOperator.CONTAINS, value="error")
        assert c.matches(self._event(msg="some error occurred")) is True
        assert c.matches(self._event(msg="all good")) is False

    def test_matches_regex(self):
        c = Condition(field="data.msg", operator=ConditionOperator.MATCHES, value=r"\d{3}")
        assert c.matches(self._event(msg="code 404")) is True
        assert c.matches(self._event(msg="no numbers")) is False

    def test_missing_field_returns_false(self):
        c = Condition(field="data.nonexistent", operator=ConditionOperator.EQ, value="x")
        assert c.matches(self._event()) is False

    def test_nested_field(self):
        c = Condition(field="data.nested", operator=ConditionOperator.EQ, value="deep")
        event = {"event_type": "q", "data": {"nested": "deep"}}
        assert c.matches(event) is True


class TestTriggerRule:
    def test_matches_no_conditions(self):
        rule = make_rule(event_type="user_query")
        assert rule.matches({"event_type": "user_query"}) is True

    def test_wrong_event_type(self):
        rule = make_rule(event_type="user_query")
        assert rule.matches({"event_type": "llm_response"}) is False

    def test_disabled_rule(self):
        rule = make_rule(enabled=False)
        assert rule.matches({"event_type": "user_query"}) is False

    def test_and_logic_all_match(self):
        c1 = Condition("data.a", ConditionOperator.EQ, 1)
        c2 = Condition("data.b", ConditionOperator.EQ, 2)
        rule = make_rule(conditions=[c1, c2], logic=ConditionLogic.AND)
        assert rule.matches({"event_type": "user_query", "data": {"a": 1, "b": 2}}) is True

    def test_and_logic_partial_match(self):
        c1 = Condition("data.a", ConditionOperator.EQ, 1)
        c2 = Condition("data.b", ConditionOperator.EQ, 2)
        rule = make_rule(conditions=[c1, c2], logic=ConditionLogic.AND)
        assert rule.matches({"event_type": "user_query", "data": {"a": 1, "b": 99}}) is False

    def test_or_logic(self):
        c1 = Condition("data.a", ConditionOperator.EQ, 1)
        c2 = Condition("data.b", ConditionOperator.EQ, 2)
        rule = make_rule(conditions=[c1, c2], logic=ConditionLogic.OR)
        assert rule.matches({"event_type": "user_query", "data": {"a": 1, "b": 99}}) is True

    def test_to_dict(self):
        rule = make_rule()
        d = rule.to_dict()
        assert d["rule_id"] == "r1"
        assert d["enabled"] is True


class TestTriggerRuleRegistry:
    @pytest.fixture
    def registry(self):
        return TriggerRuleRegistry()

    def test_register_and_get(self, registry):
        rule = make_rule()
        registry.register_rule(rule)
        assert registry.get_rule("r1") is rule

    def test_get_unknown(self, registry):
        assert registry.get_rule("nope") is None

    def test_unregister(self, registry):
        registry.register_rule(make_rule())
        assert registry.unregister_rule("r1") is True
        assert registry.get_rule("r1") is None

    def test_unregister_nonexistent(self, registry):
        assert registry.unregister_rule("nope") is False

    def test_list_rules(self, registry):
        registry.register_rule(make_rule("r1"))
        registry.register_rule(make_rule("r2", enabled=False))
        assert len(registry.list_rules()) == 2
        assert len(registry.list_rules(enabled_only=True)) == 1

    def test_find_matching_rules(self, registry):
        registry.register_rule(make_rule("r1", event_type="user_query"))
        registry.register_rule(make_rule("r2", event_type="llm_response"))
        matches = registry.find_matching_rules({"event_type": "user_query"})
        assert len(matches) == 1
        assert matches[0].rule_id == "r1"

    def test_enable_disable(self, registry):
        registry.register_rule(make_rule("r1", enabled=False))
        assert registry.enable_rule("r1") is True
        assert registry.get_rule("r1").enabled is True
        assert registry.disable_rule("r1") is True
        assert registry.get_rule("r1").enabled is False

    def test_enable_disable_nonexistent(self, registry):
        assert registry.enable_rule("nope") is False
        assert registry.disable_rule("nope") is False


class TestTaskScheduler:
    # start() blocks forever (gather of workers), so we test internals directly

    @pytest.mark.asyncio
    async def test_schedule_task_queues(self):
        from core.scheduling.task_scheduler import TaskScheduler
        scheduler = TaskScheduler(max_concurrent=2)

        async def action(event): return "ok"

        task_id = await scheduler.schedule_task("rule1", {"x": 1}, action, task_id="t1")
        assert task_id == "t1"
        assert scheduler.pending_tasks.qsize() == 1

    @pytest.mark.asyncio
    async def test_execute_task_success(self):
        from core.scheduling.task_scheduler import TaskScheduler, TaskStatus
        scheduler = TaskScheduler()

        async def action(event): return event["x"] * 2

        await scheduler.schedule_task("rule1", {"x": 5}, action, task_id="t1")
        task = await scheduler.pending_tasks.get()
        await scheduler._execute_task(task)

        assert task.status == TaskStatus.COMPLETED
        assert task.result == 10
        assert scheduler.get_task("t1") is task

    @pytest.mark.asyncio
    async def test_execute_task_failure_retries(self):
        from core.scheduling.task_scheduler import TaskScheduler, TaskStatus
        scheduler = TaskScheduler()

        async def action(event): raise ValueError("boom")

        await scheduler.schedule_task("rule1", {}, action, task_id="t2")
        task = await scheduler.pending_tasks.get()
        task.max_retries = 1
        await scheduler._execute_task(task)

        assert task.status == TaskStatus.RETRYING
        assert task.retry_count == 1
        assert scheduler.pending_tasks.qsize() == 1

    @pytest.mark.asyncio
    async def test_execute_task_permanent_failure(self):
        from core.scheduling.task_scheduler import TaskScheduler, TaskStatus
        scheduler = TaskScheduler()

        async def action(event): raise ValueError("boom")

        await scheduler.schedule_task("rule1", {}, action, task_id="t3")
        task = await scheduler.pending_tasks.get()
        task.max_retries = 0
        await scheduler._execute_task(task)

        assert task.status == TaskStatus.FAILED
        assert "boom" in task.error

    def test_get_stats(self):
        from core.scheduling.task_scheduler import TaskScheduler
        scheduler = TaskScheduler(max_concurrent=3)
        stats = scheduler.get_stats()
        assert stats["pending_tasks"] == 0
        assert stats["active_tasks"] == 0
        assert stats["max_concurrent"] == 3

    def test_get_task_unknown(self):
        from core.scheduling.task_scheduler import TaskScheduler
        assert TaskScheduler().get_task("nope") is None
