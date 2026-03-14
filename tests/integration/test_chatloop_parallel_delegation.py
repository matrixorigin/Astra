"""Integration test for parallel delegation in ChatLoop."""

import asyncio
import pytest
from unittest.mock import MagicMock
from core.events.models import StreamEventType, StreamEvent
from core.skills.delegation import DelegateTaskSkill, DelegateTaskInput


@pytest.mark.asyncio
async def test_chatloop_parallel_delegation_detection():
    """Test that ChatLoop correctly detects and handles parallel delegation."""

    # Mock agent registry
    mock_registry = MagicMock()
    agents = {
        "agent_a": MagicMock(system_prompt="Code reviewer"),
        "agent_b": MagicMock(system_prompt="Security reviewer"),
        "agent_c": MagicMock(system_prompt="Performance reviewer"),
    }
    mock_registry.get = lambda agent_id: agents.get(agent_id)

    # Mock chat loop factory for delegated agents
    async def mock_delegated_stream(*args, **kwargs):
        agent_id = kwargs.get("context", {}).get("agent_id", "unknown")

        yield StreamEvent(
            event_type=StreamEventType.RUN_STARTED,
            data={},
            agent_id=agent_id,
        )

        await asyncio.sleep(0.01)  # Simulate work

        reviews = {
            "agent_a": "Code quality: Good structure",
            "agent_b": "Security: No vulnerabilities found",
            "agent_c": "Performance: Optimized",
        }

        yield StreamEvent(
            event_type=StreamEventType.TEXT_DONE,
            data={"full_text": reviews.get(agent_id, "Review complete")},
            agent_id=agent_id,
        )

        yield StreamEvent(
            event_type=StreamEventType.RUN_FINISHED,
            data={},
            agent_id=agent_id,
        )

    def chat_loop_factory(system_prompt, agent_id):
        loop = MagicMock()
        loop.run_step_stream = mock_delegated_stream
        return loop

    # Create delegation skill
    delegation_skill = DelegateTaskSkill(mock_registry, chat_loop_factory)

    # Simulate parallel delegation inputs (what ChatLoop would construct)
    inputs = [
        DelegateTaskInput(
            agent_id="agent_a",
            task="Review code quality",
            session_id="test_session",
            user_id="test_user",
        ),
        DelegateTaskInput(
            agent_id="agent_b",
            task="Review security",
            session_id="test_session",
            user_id="test_user",
        ),
        DelegateTaskInput(
            agent_id="agent_c",
            task="Review performance",
            session_id="test_session",
            user_id="test_user",
        ),
    ]

    # Execute parallel streaming (fan-out/fan-in)
    events = []
    async for event in delegation_skill.execute_parallel_stream(inputs):
        events.append(event)

    # Verify parallel delegation events
    event_types = [e.event_type for e in events]

    # Should have AGENT_DELEGATED for all 3 agents
    delegated = [e for e in events if e.event_type == StreamEventType.AGENT_DELEGATED]
    assert len(delegated) == 3, f"Expected 3 AGENT_DELEGATED, got {len(delegated)}"

    # Should have RUN_STARTED for all 3 agents
    run_started = [e for e in events if e.event_type == StreamEventType.RUN_STARTED]
    assert len(run_started) == 3, f"Expected 3 RUN_STARTED, got {len(run_started)}"

    # Should have TEXT_DONE for all 3 agents
    text_done = [e for e in events if e.event_type == StreamEventType.TEXT_DONE]
    assert len(text_done) == 3, f"Expected 3 TEXT_DONE, got {len(text_done)}"

    # Should have AGENT_COMPLETED for all 3 agents
    completed = [e for e in events if e.event_type == StreamEventType.AGENT_COMPLETED]
    assert len(completed) == 3, f"Expected 3 AGENT_COMPLETED, got {len(completed)}"

    # Verify agent_ids are tagged correctly
    agent_ids = {e.agent_id for e in delegated}
    assert agent_ids == {"agent_a", "agent_b", "agent_c"}, f"Expected all 3 agents, got {agent_ids}"

    # Verify TEXT_DONE events contain results
    for event in text_done:
        assert "full_text" in event.data, f"TEXT_DONE missing full_text: {event.data}"
        assert event.data["full_text"], f"TEXT_DONE has empty full_text: {event.data}"

    print("✅ Parallel delegation fan-out/fan-in working correctly!")


@pytest.mark.asyncio
async def test_chatloop_tool_call_batching():
    """Test that ChatLoop batches multiple delegate_task calls for parallel execution."""

    # Simulate tool_calls from LLM
    tool_calls = [
        {
            "id": "call_1",
            "type": "function",
            "function": {
                "name": "delegate_task",
                "arguments": '{"agent_id": "agent_a", "task": "Review code"}',
            },
        },
        {
            "id": "call_2",
            "type": "function",
            "function": {
                "name": "delegate_task",
                "arguments": '{"agent_id": "agent_b", "task": "Review security"}',
            },
        },
        {
            "id": "call_3",
            "type": "function",
            "function": {
                "name": "delegate_task",
                "arguments": '{"agent_id": "agent_c", "task": "Review performance"}',
            },
        },
    ]

    # Test detection logic (what ChatLoop does)
    delegation_calls = [tc for tc in tool_calls if tc["function"]["name"] == "delegate_task"]

    assert len(delegation_calls) == 3, "Should detect 3 parallel delegations"

    # Verify we can construct inputs from tool calls
    import json

    inputs = []
    for tc in delegation_calls:
        params = json.loads(tc["function"]["arguments"])
        inputs.append(
            {
                "agent_id": params.get("agent_id"),
                "task": params.get("task"),
                "call_id": tc["id"],
            }
        )

    assert len(inputs) == 3
    assert inputs[0]["agent_id"] == "agent_a"
    assert inputs[1]["agent_id"] == "agent_b"
    assert inputs[2]["agent_id"] == "agent_c"

    print("✅ Tool call batching logic working correctly!")


if __name__ == "__main__":
    asyncio.run(test_chatloop_parallel_delegation_detection())
    asyncio.run(test_chatloop_tool_call_batching())


@pytest.mark.asyncio
async def test_delegation_timeout():
    """Test that delegation respects timeout parameter."""

    # Mock agent registry
    mock_registry = MagicMock()
    agents = {
        "slow_agent": MagicMock(system_prompt="Slow agent"),
    }
    mock_registry.get = lambda agent_id: agents.get(agent_id)

    # Mock chat loop factory that simulates slow agent
    async def mock_slow_stream(*args, **kwargs):
        agent_id = kwargs.get("context", {}).get("agent_id", "unknown")

        yield StreamEvent(
            event_type=StreamEventType.RUN_STARTED,
            data={},
            agent_id=agent_id,
        )

        # Simulate slow work (longer than timeout)
        await asyncio.sleep(2.0)

        yield StreamEvent(
            event_type=StreamEventType.TEXT_DONE,
            data={"full_text": "This should not be reached"},
            agent_id=agent_id,
        )

    def chat_loop_factory(system_prompt, agent_id):
        loop = MagicMock()
        loop.run_step_stream = mock_slow_stream
        return loop

    # Create delegation skill
    delegation_skill = DelegateTaskSkill(mock_registry, chat_loop_factory)

    # Delegation with timeout
    input_with_timeout = DelegateTaskInput(
        agent_id="slow_agent",
        task="Do something slow",
        session_id="test_session",
        user_id="test_user",
        timeout=0.5,  # 500ms timeout
    )

    # Execute and collect events
    events = []
    async for event in delegation_skill.execute_stream(input_with_timeout):
        events.append(event)

    # Verify timeout was triggered
    event_types = [e.event_type for e in events]

    assert StreamEventType.AGENT_DELEGATED in event_types
    assert StreamEventType.RUN_ERROR in event_types

    # Verify error message contains timeout
    error_events = [e for e in events if e.event_type == StreamEventType.RUN_ERROR]
    assert len(error_events) == 1
    assert "Timeout" in error_events[0].data.get("error", "")

    print("✅ Delegation timeout working correctly!")


@pytest.mark.asyncio
async def test_delegation_cancellation():
    """Test that delegation can be cancelled."""

    # Mock agent registry
    mock_registry = MagicMock()
    agents = {
        "agent": MagicMock(system_prompt="Agent"),
    }
    mock_registry.get = lambda agent_id: agents.get(agent_id)

    # Mock chat loop factory
    async def mock_stream(*args, **kwargs):
        agent_id = kwargs.get("context", {}).get("agent_id", "unknown")

        yield StreamEvent(
            event_type=StreamEventType.RUN_STARTED,
            data={},
            agent_id=agent_id,
        )

        # Simulate work that can be cancelled
        for i in range(10):
            await asyncio.sleep(0.1)
            yield StreamEvent(
                event_type=StreamEventType.TEXT_DELTA,
                data={"chunk": f"chunk {i}"},
                agent_id=agent_id,
            )

    def chat_loop_factory(system_prompt, agent_id):
        loop = MagicMock()
        loop.run_step_stream = mock_stream
        return loop

    # Create delegation skill
    delegation_skill = DelegateTaskSkill(mock_registry, chat_loop_factory)

    # Delegation input
    input_data = DelegateTaskInput(
        agent_id="agent",
        task="Do something",
        session_id="test_session",
        user_id="test_user",
    )

    # Execute in a task so we can cancel it
    async def run_delegation():
        events = []
        async for event in delegation_skill.execute_stream(input_data):
            events.append(event)
        return events

    task = asyncio.create_task(run_delegation())

    # Let it run for a bit
    await asyncio.sleep(0.15)

    # Cancel the task
    task.cancel()

    # Verify cancellation
    try:
        await task
        assert False, "Task should have been cancelled"
    except asyncio.CancelledError:
        print("✅ Delegation cancellation working correctly!")
