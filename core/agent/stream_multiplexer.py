"""Stream multiplexer for multi-agent coordination.

Merges streams from multiple parallel agents into a single output stream.
"""

import asyncio
from collections.abc import AsyncIterator

from core.events.models import StreamEvent
from core.logging_config import get_logger

logger = get_logger(__name__)


class StreamMultiplexer:
    """Merge multiple agent streams into one."""

    async def merge_streams(
        self, streams: dict[str, AsyncIterator[StreamEvent]]
    ) -> AsyncIterator[StreamEvent]:
        """Merge multiple agent streams.

        Args:
            streams: Dict of agent_id -> stream iterator

        Yields:
            StreamEvent with agent_id tagged
        """
        queue: asyncio.Queue[StreamEvent | None] = asyncio.Queue()

        async def producer(agent_id: str, stream: AsyncIterator[StreamEvent]):
            """Produce events from one agent stream."""
            try:
                async for event in stream:
                    # Tag with agent_id if not already set
                    if event.agent_id is None:
                        event.agent_id = agent_id
                    await queue.put(event)
            except Exception as e:
                logger.error(f"Error in agent {agent_id} stream: {e}")

        # Start all producers
        tasks = [
            asyncio.create_task(producer(agent_id, stream)) for agent_id, stream in streams.items()
        ]

        # Add sentinel task
        async def sentinel():
            await asyncio.gather(*tasks, return_exceptions=True)
            await queue.put(None)  # Signal completion

        asyncio.create_task(sentinel())

        # Consume merged stream
        while True:
            event = await queue.get()
            if event is None:  # All streams done
                break
            yield event


async def merge_parallel_agents(
    agent_streams: dict[str, AsyncIterator[StreamEvent]],
) -> AsyncIterator[StreamEvent]:
    """Convenience function to merge parallel agent streams.

    Args:
        agent_streams: Dict of agent_id -> stream

    Yields:
        Merged stream events with agent_id tags
    """
    multiplexer = StreamMultiplexer()
    async for event in multiplexer.merge_streams(agent_streams):
        yield event
