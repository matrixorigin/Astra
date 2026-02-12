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
        if not streams:
            logger.warning("No streams provided to multiplexer")
            return

        queue: asyncio.Queue[StreamEvent | None] = asyncio.Queue()

        async def producer(agent_id: str, stream: AsyncIterator[StreamEvent]):
            """Produce events from one agent stream."""
            if not agent_id or not agent_id.strip():
                logger.error("Empty agent_id in producer")
                return

            try:
                async for event in stream:
                    if event is None:
                        continue
                    # Tag with agent_id if not already set
                    if event.agent_id is None:
                        event.agent_id = agent_id
                    await queue.put(event)
            except asyncio.CancelledError:
                logger.info(f"Agent {agent_id} stream cancelled")
                raise
            except Exception as e:
                logger.error(f"Error in agent {agent_id} stream: {e}")

        # Start all producers
        tasks = []
        for agent_id, stream in streams.items():
            if stream is None:
                logger.warning(f"Null stream for agent {agent_id}")
                continue
            task = asyncio.create_task(producer(agent_id, stream))
            tasks.append(task)

        if not tasks:
            logger.warning("No valid streams to multiplex")
            return

        # Add sentinel task
        async def sentinel():
            await asyncio.gather(*tasks, return_exceptions=True)
            await queue.put(None)  # Signal completion

        asyncio.create_task(sentinel())

        # Consume merged stream
        try:
            while True:
                event = await queue.get()
                if event is None:  # All streams done
                    break
                yield event
        except asyncio.CancelledError:
            logger.info("Multiplexer cancelled")
            # Cancel all producer tasks
            for task in tasks:
                task.cancel()
            raise


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
