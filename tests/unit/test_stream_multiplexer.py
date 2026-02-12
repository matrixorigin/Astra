"""Test stream multiplexer for multi-agent coordination."""

import asyncio

import pytest

from core.agent.stream_multiplexer import StreamMultiplexer, merge_parallel_agents
from core.events.models import StreamEvent, StreamEventType


async def mock_agent_stream(agent_id: str, num_events: int):
    """Mock agent stream that yields events."""
    for i in range(num_events):
        await asyncio.sleep(0.01)  # Simulate work
        yield StreamEvent(
            event_type=StreamEventType.TEXT_DELTA,
            data={"chunk": f"{agent_id} event {i}"},
            agent_id=agent_id,
        )


@pytest.mark.asyncio
async def test_multiplexer_single_agent():
    """Test multiplexer with single agent."""
    multiplexer = StreamMultiplexer()

    streams = {"agent_1": mock_agent_stream("agent_1", 3)}

    events = []
    async for event in multiplexer.merge_streams(streams):
        events.append(event)

    assert len(events) == 3
    assert all(e.agent_id == "agent_1" for e in events)


@pytest.mark.asyncio
async def test_multiplexer_multiple_agents():
    """Test multiplexer with multiple parallel agents."""
    multiplexer = StreamMultiplexer()

    streams = {
        "agent_1": mock_agent_stream("agent_1", 2),
        "agent_2": mock_agent_stream("agent_2", 2),
        "agent_3": mock_agent_stream("agent_3", 2),
    }

    events = []
    async for event in multiplexer.merge_streams(streams):
        events.append(event)

    # Should have all events from all agents
    assert len(events) == 6

    # Check agent_id tags
    agent_1_events = [e for e in events if e.agent_id == "agent_1"]
    agent_2_events = [e for e in events if e.agent_id == "agent_2"]
    agent_3_events = [e for e in events if e.agent_id == "agent_3"]

    assert len(agent_1_events) == 2
    assert len(agent_2_events) == 2
    assert len(agent_3_events) == 2


@pytest.mark.asyncio
async def test_multiplexer_preserves_event_data():
    """Test that multiplexer preserves event data."""
    multiplexer = StreamMultiplexer()

    streams = {"agent_1": mock_agent_stream("agent_1", 2)}

    events = []
    async for event in multiplexer.merge_streams(streams):
        events.append(event)

    # Check data is preserved
    assert events[0].data["chunk"] == "agent_1 event 0"
    assert events[1].data["chunk"] == "agent_1 event 1"


@pytest.mark.asyncio
async def test_merge_parallel_agents_convenience():
    """Test convenience function for merging streams."""
    streams = {
        "agent_1": mock_agent_stream("agent_1", 2),
        "agent_2": mock_agent_stream("agent_2", 2),
    }

    events = []
    async for event in merge_parallel_agents(streams):
        events.append(event)

    assert len(events) == 4


@pytest.mark.asyncio
async def test_multiplexer_with_different_event_types():
    """Test multiplexer with different event types."""

    async def mixed_stream(agent_id: str):
        yield StreamEvent(
            event_type=StreamEventType.RUN_STARTED,
            data={"query": "test"},
            agent_id=agent_id,
        )
        yield StreamEvent(
            event_type=StreamEventType.TEXT_DELTA,
            data={"chunk": "hello"},
            agent_id=agent_id,
        )
        yield StreamEvent(
            event_type=StreamEventType.RUN_FINISHED,
            data={},
            agent_id=agent_id,
        )

    multiplexer = StreamMultiplexer()
    streams = {"agent_1": mixed_stream("agent_1")}

    events = []
    async for event in multiplexer.merge_streams(streams):
        events.append(event)

    assert len(events) == 3
    assert events[0].event_type == StreamEventType.RUN_STARTED
    assert events[1].event_type == StreamEventType.TEXT_DELTA
    assert events[2].event_type == StreamEventType.RUN_FINISHED


@pytest.mark.asyncio
async def test_multiplexer_tags_untagged_events():
    """Test that multiplexer tags events without agent_id."""

    async def untagged_stream():
        yield StreamEvent(
            event_type=StreamEventType.TEXT_DELTA,
            data={"chunk": "test"},
            agent_id=None,  # No agent_id
        )

    multiplexer = StreamMultiplexer()
    streams = {"agent_1": untagged_stream()}

    events = []
    async for event in multiplexer.merge_streams(streams):
        events.append(event)

    # Should be tagged with agent_1
    assert events[0].agent_id == "agent_1"


@pytest.mark.asyncio
async def test_multiplexer_empty_streams():
    """Test multiplexer with empty streams dict."""
    multiplexer = StreamMultiplexer()

    events = []
    async for event in multiplexer.merge_streams({}):
        events.append(event)

    assert len(events) == 0


@pytest.mark.asyncio
async def test_multiplexer_null_stream():
    """Test multiplexer with null stream."""
    multiplexer = StreamMultiplexer()

    streams = {"agent_1": None}

    events = []
    async for event in multiplexer.merge_streams(streams):
        events.append(event)

    # Should handle gracefully
    assert len(events) == 0


@pytest.mark.asyncio
async def test_multiplexer_stream_with_error():
    """Test multiplexer when one stream raises error."""

    async def error_stream():
        yield StreamEvent(
            event_type=StreamEventType.TEXT_DELTA,
            data={"chunk": "before error"},
            agent_id="agent_1",
        )
        raise ValueError("Stream error")

    async def normal_stream():
        yield StreamEvent(
            event_type=StreamEventType.TEXT_DELTA,
            data={"chunk": "normal"},
            agent_id="agent_2",
        )

    multiplexer = StreamMultiplexer()
    streams = {"agent_1": error_stream(), "agent_2": normal_stream()}

    events = []
    async for event in multiplexer.merge_streams(streams):
        events.append(event)

    # Should get events from both streams (error doesn't crash multiplexer)
    assert len(events) >= 1


if __name__ == "__main__":
    pytest.main([__file__, "-v"])
