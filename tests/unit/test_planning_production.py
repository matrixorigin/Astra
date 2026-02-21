"""Tests for planning system production hardening.

Covers: cross-session plan restore, constraint enforcement via PlanConstraints,
plan lifecycle event logging, and revision history persistence.
"""

import json

import pytest
from unittest.mock import AsyncMock, MagicMock, patch

from core.agent.chat_loop import ChatLoop
from core.agent.planner import PlanConstraints, PlanStatus
from core.events.models import StreamEventType


def _make_chat_loop(**overrides):
    """Create ChatLoop with mocked dependencies."""
    defaults = dict(
        selector=MagicMock(),
        executor=MagicMock(),
        llm_client=MagicMock(),
        event_logger=MagicMock(),
        context_manager=MagicMock(),
        firewall=MagicMock(),
    )
    defaults.update(overrides)
    return ChatLoop(**defaults)


def _make_step(step_id="step_1", skill_hint=None, description="do something"):
    s = MagicMock()
    s.step_id = step_id
    s.skill_hint = skill_hint
    s.description = description
    s.status = PlanStatus.PENDING
    s.result = None
    return s


async def _collect(gen):
    return [e async for e in gen]


# ── Cross-session plan restore ────────────────────────────────────


@pytest.mark.asyncio
async def test_resumes_plan_from_events_when_available():
    """When restore_plan_from_events returns a plan, skip create_plan."""
    step = _make_step()
    restored_plan = MagicMock()
    restored_plan.plan_id = "plan_restored"
    restored_plan.steps = [step]
    restored_plan.model_dump.return_value = {"plan_id": "plan_restored"}
    restored_plan.model_dump_json.return_value = '{"plan_id":"plan_restored"}'

    loop = _make_chat_loop()

    with patch("core.agent.chat_loop.Planner") as MockPlanner, \
         patch("core.agent.chat_loop.restore_plan_from_events", return_value=restored_plan):
        planner = MagicMock()
        planner.check_constraints.return_value = (True, None)
        planner.get_next_steps.side_effect = [[step], []]
        planner.constraints = PlanConstraints(max_revisions=1)
        planner.log_step_start.return_value = None
        planner.log_step_done.return_value = None
        planner.log_plan_completed.return_value = None
        MockPlanner.return_value = planner

        events = await _collect(loop.run_step_with_planning(
            "goal", "sess", "user",
        ))

    # create_plan should NOT have been called
    planner.create_plan.assert_not_called()
    # PLAN_CREATED event should include resumed=True
    plan_created = [e for e in events if e.event_type == StreamEventType.PLAN_CREATED]
    assert len(plan_created) == 1
    assert plan_created[0].data["resumed"] is True


@pytest.mark.asyncio
async def test_creates_new_plan_when_no_prior_exists():
    """When restore returns None, create_plan is called."""
    step = _make_step()
    new_plan = MagicMock()
    new_plan.plan_id = "plan_new"
    new_plan.steps = [step]
    new_plan.model_dump.return_value = {"plan_id": "plan_new"}
    new_plan.model_dump_json.return_value = '{"plan_id":"plan_new"}'

    loop = _make_chat_loop()

    with patch("core.agent.chat_loop.Planner") as MockPlanner, \
         patch("core.agent.chat_loop.restore_plan_from_events", return_value=None):
        planner = MagicMock()
        planner.create_plan = AsyncMock(return_value=new_plan)
        planner.check_constraints.return_value = (True, None)
        planner.get_next_steps.side_effect = [[step], []]
        planner.constraints = PlanConstraints(max_revisions=1)
        planner.log_step_start.return_value = None
        planner.log_step_done.return_value = None
        planner.log_plan_completed.return_value = None
        MockPlanner.return_value = planner

        events = await _collect(loop.run_step_with_planning(
            "goal", "sess", "user",
        ))

    planner.create_plan.assert_called_once()
    plan_created = [e for e in events if e.event_type == StreamEventType.PLAN_CREATED]
    assert plan_created[0].data["resumed"] is False


# ── Constraint enforcement ────────────────────────────────────────


@pytest.mark.asyncio
async def test_max_revisions_from_constraints_not_llm_config():
    """PAOR loop should use constraints.max_revisions, not llm.config."""
    step = _make_step()
    plan = MagicMock()
    plan.plan_id = "p1"
    plan.steps = [step]
    plan.model_dump.return_value = {}
    plan.model_dump_json.return_value = "{}"

    loop = _make_chat_loop()
    # Set llm.config to a different value — should be ignored
    loop.llm.config = {"max_revisions": 99}

    with patch("core.agent.chat_loop.Planner") as MockPlanner, \
         patch("core.agent.chat_loop.restore_plan_from_events", return_value=None):
        planner = MagicMock()
        planner.create_plan = AsyncMock(return_value=plan)
        planner.check_constraints.return_value = (True, None)
        # Never return next steps → loop exits immediately
        planner.get_next_steps.return_value = []
        planner.constraints = PlanConstraints(max_revisions=2)
        planner.log_plan_completed.return_value = None
        MockPlanner.return_value = planner

        await _collect(loop.run_step_with_planning("g", "s", "u"))

    # get_next_steps called once (not 99 times)
    assert planner.get_next_steps.call_count == 1


@pytest.mark.asyncio
async def test_constraint_violation_logs_plan_failed():
    """When check_constraints fails, log_plan_failed is called and RUN_ERROR emitted."""
    plan = MagicMock()
    plan.plan_id = "p1"
    plan.steps = []
    plan.model_dump_json.return_value = "{}"

    loop = _make_chat_loop()

    with patch("core.agent.chat_loop.Planner") as MockPlanner, \
         patch("core.agent.chat_loop.restore_plan_from_events", return_value=None):
        planner = MagicMock()
        planner.create_plan = AsyncMock(return_value=plan)
        planner.check_constraints.return_value = (False, "too many steps")
        planner.constraints = PlanConstraints()
        MockPlanner.return_value = planner

        events = await _collect(loop.run_step_with_planning("g", "s", "u"))

    planner.log_plan_failed.assert_called_once()
    assert "too many steps" in planner.log_plan_failed.call_args[0][3]
    error_events = [e for e in events if e.event_type == StreamEventType.RUN_ERROR]
    assert len(error_events) == 1


# ── Lifecycle event logging ───────────────────────────────────────


@pytest.mark.asyncio
async def test_step_lifecycle_events_logged():
    """log_step_start and log_step_done called for each executed step."""
    s1 = _make_step("s1")
    s2 = _make_step("s2")
    plan = MagicMock()
    plan.plan_id = "p1"
    plan.steps = [s1, s2]
    plan.model_dump.return_value = {}
    plan.model_dump_json.return_value = "{}"

    loop = _make_chat_loop()

    with patch("core.agent.chat_loop.Planner") as MockPlanner, \
         patch("core.agent.chat_loop.restore_plan_from_events", return_value=None):
        planner = MagicMock()
        planner.create_plan = AsyncMock(return_value=plan)
        planner.check_constraints.return_value = (True, None)
        planner.get_next_steps.side_effect = [[s1, s2], []]
        planner.constraints = PlanConstraints(max_revisions=1)
        planner.log_step_start.return_value = None
        planner.log_step_done.return_value = None
        planner.log_plan_completed.return_value = None
        MockPlanner.return_value = planner

        await _collect(loop.run_step_with_planning("g", "s", "u"))

    assert planner.log_step_start.call_count == 2
    assert planner.log_step_done.call_count == 2
    # Verify step IDs
    start_ids = [c[0][0].step_id for c in planner.log_step_start.call_args_list]
    assert start_ids == ["s1", "s2"]


@pytest.mark.asyncio
async def test_plan_completed_logged():
    """log_plan_completed called after PAOR loop finishes."""
    step = _make_step()
    plan = MagicMock()
    plan.plan_id = "p1"
    plan.steps = [step]
    plan.model_dump.return_value = {}
    plan.model_dump_json.return_value = "{}"

    loop = _make_chat_loop()

    with patch("core.agent.chat_loop.Planner") as MockPlanner, \
         patch("core.agent.chat_loop.restore_plan_from_events", return_value=None):
        planner = MagicMock()
        planner.create_plan = AsyncMock(return_value=plan)
        planner.check_constraints.return_value = (True, None)
        planner.get_next_steps.side_effect = [[step], []]
        planner.constraints = PlanConstraints(max_revisions=1)
        planner.log_step_start.return_value = None
        planner.log_step_done.return_value = None
        planner.log_plan_completed.return_value = None
        MockPlanner.return_value = planner

        await _collect(loop.run_step_with_planning("g", "s", "u"))

    planner.log_plan_completed.assert_called_once()


# ── Revision history persistence ──────────────────────────────────


@pytest.mark.asyncio
async def test_revision_logged_via_planner():
    """When reflect returns a revised plan, log_plan_revised is called."""
    s1 = _make_step("s1")
    s2 = _make_step("s2")
    plan = MagicMock()
    plan.plan_id = "p1"
    plan.steps = [s1, s2]
    plan.model_dump.return_value = {}
    plan.model_dump_json.return_value = "{}"

    revised_step = _make_step("s3")
    revised_plan = MagicMock()
    revised_plan.plan_id = "p1_rev_1"
    revised_plan.steps = [revised_step]
    revised_plan.model_dump.return_value = {"plan_id": "p1_rev_1"}
    revised_plan.model_dump_json.return_value = '{"plan_id":"p1_rev_1"}'

    loop = _make_chat_loop()

    with patch("core.agent.chat_loop.Planner") as MockPlanner, \
         patch("core.agent.chat_loop.restore_plan_from_events", return_value=None):
        planner = MagicMock()
        planner.create_plan = AsyncMock(return_value=plan)
        planner.check_constraints.return_value = (True, None)
        # Round 1: only s1 ready → execute s1 → s2 still pending → reflect → revise
        # Round 2: revised plan has s3 ready → execute → all done → break
        planner.get_next_steps.side_effect = [[s1], [revised_step], []]
        planner.reflect = AsyncMock(return_value=("needs revision", revised_plan))
        planner.constraints = PlanConstraints(max_revisions=3)
        planner.log_step_start.return_value = None
        planner.log_step_done.return_value = None
        planner.log_plan_revised.return_value = None
        planner.log_plan_completed.return_value = None
        MockPlanner.return_value = planner

        events = await _collect(loop.run_step_with_planning("g", "s", "u"))

    planner.log_plan_revised.assert_called_once()
    assert planner.log_plan_revised.call_args[0][0].plan_id == "p1_rev_1"
    revised_events = [e for e in events if e.event_type == StreamEventType.PLAN_REVISED]
    assert len(revised_events) == 1


# ── Planner wired with event_logger and db ────────────────────────


@pytest.mark.asyncio
async def test_planner_receives_event_logger_and_db():
    """Planner is constructed with event_logger and db from ChatLoop."""
    loop = _make_chat_loop()

    with patch("core.agent.chat_loop.Planner") as MockPlanner, \
         patch("core.agent.chat_loop.restore_plan_from_events", return_value=None):
        plan = MagicMock()
        plan.plan_id = "p1"
        plan.steps = []
        plan.model_dump.return_value = {}
        plan.model_dump_json.return_value = "{}"

        planner = MagicMock()
        planner.create_plan = AsyncMock(return_value=plan)
        planner.check_constraints.return_value = (True, None)
        planner.get_next_steps.return_value = []
        planner.constraints = PlanConstraints(max_revisions=1)
        planner.log_plan_completed.return_value = None
        MockPlanner.return_value = planner

        await _collect(loop.run_step_with_planning("g", "s", "u"))

    # Verify Planner() was called with event_logger and db
    _, kwargs = MockPlanner.call_args
    assert kwargs["event_logger"] is loop.event_logger
    assert kwargs["db"] is loop.event_logger.session


@pytest.mark.asyncio
async def test_completed_plan_not_resumed():
    """A fully-completed plan should not be resumed — create a new one instead."""
    done_step = _make_step("s1")
    done_step.status = PlanStatus.COMPLETED
    old_plan = MagicMock()
    old_plan.plan_id = "plan_old"
    old_plan.steps = [done_step]

    new_step = _make_step("s_new")
    new_plan = MagicMock()
    new_plan.plan_id = "plan_fresh"
    new_plan.steps = [new_step]
    new_plan.model_dump.return_value = {}
    new_plan.model_dump_json.return_value = "{}"

    loop = _make_chat_loop()

    with patch("core.agent.chat_loop.Planner") as MockPlanner, \
         patch("core.agent.chat_loop.restore_plan_from_events", return_value=old_plan):
        planner = MagicMock()
        planner.create_plan = AsyncMock(return_value=new_plan)
        planner.check_constraints.return_value = (True, None)
        planner.get_next_steps.side_effect = [[new_step], []]
        planner.constraints = PlanConstraints(max_revisions=1)
        planner.log_step_start.return_value = None
        planner.log_step_done.return_value = None
        planner.log_plan_completed.return_value = None
        MockPlanner.return_value = planner

        events = await _collect(loop.run_step_with_planning("g", "s", "u"))

    # Should have created a new plan, not resumed the completed one
    planner.create_plan.assert_called_once()
    plan_created = [e for e in events if e.event_type == StreamEventType.PLAN_CREATED]
    assert plan_created[0].data["resumed"] is False


@pytest.mark.asyncio
async def test_restore_failure_falls_back_to_new_plan():
    """If restore_plan_from_events raises, gracefully fall back to create_plan."""
    step = _make_step()
    new_plan = MagicMock()
    new_plan.plan_id = "plan_fallback"
    new_plan.steps = [step]
    new_plan.model_dump.return_value = {}
    new_plan.model_dump_json.return_value = "{}"

    loop = _make_chat_loop()

    with patch("core.agent.chat_loop.Planner") as MockPlanner, \
         patch("core.agent.chat_loop.restore_plan_from_events", side_effect=RuntimeError("DB down")):
        planner = MagicMock()
        planner.create_plan = AsyncMock(return_value=new_plan)
        planner.check_constraints.return_value = (True, None)
        planner.get_next_steps.side_effect = [[step], []]
        planner.constraints = PlanConstraints(max_revisions=1)
        planner.log_step_start.return_value = None
        planner.log_step_done.return_value = None
        planner.log_plan_completed.return_value = None
        MockPlanner.return_value = planner

        events = await _collect(loop.run_step_with_planning("g", "s", "u"))

    # Should not crash — falls back to create_plan
    planner.create_plan.assert_called_once()
    plan_created = [e for e in events if e.event_type == StreamEventType.PLAN_CREATED]
    assert plan_created[0].data["resumed"] is False


# ── Planner fallback fixes ────────────────────────────────────────


@pytest.mark.asyncio
async def test_fallback_plan_has_unique_id():
    """JSON parse failure should produce unique plan_id, not hardcoded."""
    from core.agent.planner import Planner

    llm = MagicMock()
    resp = MagicMock()
    resp.content = "not valid json"
    llm.chat.return_value = resp

    p = Planner(llm)
    plan1 = await p.create_plan(goal="g1")
    plan2 = await p.create_plan(goal="g2")
    assert plan1.plan_id != plan2.plan_id
    assert plan1.plan_id.startswith("plan_")


@pytest.mark.asyncio
async def test_validation_error_fallback_logs_event():
    """ValidationError fallback should log plan_created event."""
    from core.agent.planner import Planner

    llm = MagicMock()
    # Return valid JSON but missing required 'goal' field → ValidationError
    resp = MagicMock()
    resp.content = json.dumps({"plan_id": "p1", "steps": [{"step_id": "s1", "description": "d"}]})
    llm.chat.return_value = resp

    el = MagicMock()
    p = Planner(llm, event_logger=el)
    plan = await p.create_plan(goal="test", user_id="u", session_id="s")

    assert plan.plan_id.startswith("plan_")
    el.create_plan_event.assert_called_once()
    call_kwargs = el.create_plan_event.call_args[1]
    assert call_kwargs["metadata"]["fallback"] is True
