"""Integration test for multi-agent delegation with ChatLoop."""

import asyncio
import pytest
from unittest.mock import MagicMock

from core.skills.delegation import DelegateTaskSkill, DelegateTaskInput
from core.events.models import StreamEvent, StreamEventType


@pytest.mark.asyncio
async def test_parallel_delegation_integration():
    """Test parallel delegation with multiple agents."""

    # Mock registry
    registry = MagicMock()

    agents = {
        "code_agent": MagicMock(system_prompt="Code review"),
        "security_agent": MagicMock(system_prompt="Security review"),
        "test_agent": MagicMock(system_prompt="Test review"),
    }

    registry.get = lambda agent_id: agents.get(agent_id)

    # Mock factory
    async def mock_stream(*args, **kwargs):
        agent_id = kwargs.get("context", {}).get("agent_id", "unknown")

        # Simulate some work
        await asyncio.sleep(0.01)

        yield StreamEvent(
            event_type=StreamEventType.RUN_STARTED,
            data={},
            agent_id=agent_id,
        )

        yield StreamEvent(
            event_type=StreamEventType.TEXT_DELTA,
            data={"chunk": f"Review from {agent_id}"},
            agent_id=agent_id,
        )

        yield StreamEvent(
            event_type=StreamEventType.TEXT_DONE,
            data={"full_text": f"Review from {agent_id}"},
            agent_id=agent_id,
        )

        yield StreamEvent(
            event_type=StreamEventType.RUN_FINISHED,
            data={},
            agent_id=agent_id,
        )

    def factory(system_prompt, agent_id):
        loop = MagicMock()
        loop.run_step_stream = mock_stream
        return loop

    # Create skill
    skill = DelegateTaskSkill(registry, factory)

    # Create parallel delegations
    inputs = [
        DelegateTaskInput(
            agent_id=agent_id,
            task=f"Review from {agent_id}",
            session_id="test_session",
            user_id="test_user",
        )
        for agent_id in ["code_agent", "security_agent", "test_agent"]
    ]

    # Execute in parallel
    events = []
    async for event in skill.execute_parallel_stream(inputs):
        events.append(event)

    # Verify all agents produced events
    agent_ids = {e.agent_id for e in events if e.agent_id and e.agent_id != "orchestrator"}
    assert agent_ids == {"code_agent", "security_agent", "test_agent"}

    # Verify aggregation
    agg_events = [e for e in events if e.event_type == StreamEventType.AGENT_PROGRESS]
    assert len(agg_events) == 1

    agg = agg_events[0].data["aggregated_results"]
    assert agg["total"] == 3
    assert agg["successful"] == 3
    assert agg["failed"] == 0

    # Verify all results collected
    assert len(agg["delegations"]) == 3
    for delegation in agg["delegations"]:
        assert delegation["success"] is True
        assert "Review from" in delegation["result"]


@pytest.mark.asyncio
async def test_sequential_delegation():
    """Test sequential delegation (pipeline pattern)."""

    # Mock registry
    registry = MagicMock()

    agents = {
        "agent_1": MagicMock(system_prompt="Agent 1"),
        "agent_2": MagicMock(system_prompt="Agent 2"),
    }

    registry.get = lambda agent_id: agents.get(agent_id)

    # Mock factory
    async def mock_stream(*args, **kwargs):
        agent_id = kwargs.get("context", {}).get("agent_id", "unknown")
        task = kwargs.get("user_input", "")

        yield StreamEvent(
            event_type=StreamEventType.RUN_STARTED,
            data={},
            agent_id=agent_id,
        )

        # Agent 2 uses result from Agent 1
        if agent_id == "agent_2" and "Result from agent_1" in task:
            result = "Processed: Result from agent_1"
        else:
            result = f"Result from {agent_id}"

        yield StreamEvent(
            event_type=StreamEventType.TEXT_DONE,
            data={"full_text": result},
            agent_id=agent_id,
        )

        yield StreamEvent(
            event_type=StreamEventType.RUN_FINISHED,
            data={},
            agent_id=agent_id,
        )

    async def mock_run_step(*args, **kwargs):
        agent_id = kwargs.get("context", {}).get("agent_id", "unknown")
        return f"Result from {agent_id}"

    def factory(system_prompt, agent_id):
        loop = MagicMock()
        loop.run_step_stream = mock_stream
        loop.run_step = mock_run_step
        return loop

    # Create skill
    skill = DelegateTaskSkill(registry, factory)

    # Sequential execution
    input1 = DelegateTaskInput(
        agent_id="agent_1",
        task="Do task 1",
        session_id="test_session",
        user_id="test_user",
    )

    result1 = await skill.execute(input1)
    assert result1.success is True
    assert "agent_1" in result1.result

    # Use result from agent_1 as input to agent_2
    input2 = DelegateTaskInput(
        agent_id="agent_2",
        task=f"Process: {result1.result}",
        session_id="test_session",
        user_id="test_user",
    )

    result2 = await skill.execute(input2)
    assert result2.success is True
    assert "agent_2" in result2.result
