"""Multi-agent stream aggregation with priority ordering.

Merge parallel agent streams into single ordered output.
"""

from __future__ import annotations

import asyncio
import logging
from dataclasses import dataclass, field
from datetime import datetime
from enum import Enum
from typing import AsyncIterator, Optional

logger = logging.getLogger(__name__)


class EventPriority(int, Enum):
    """Event priority for ordering."""

    CRITICAL = 0  # Errors, cancellations
    HIGH = 1  # Agent decisions, tool calls
    NORMAL = 2  # Progress, intermediate results
    LOW = 3  # Metadata, diagnostics


@dataclass
class PrioritizedEvent:
    """Event with priority and source."""

    event_type: str
    data: dict
    agent_id: str
    priority: EventPriority = EventPriority.NORMAL
    timestamp: datetime = field(default_factory=datetime.now)
    sequence: int = 0  # Global sequence number

    def to_dict(self) -> dict:
        """Serialize to dict."""
        return {
            "event_type": self.event_type,
            "data": self.data,
            "agent_id": self.agent_id,
            "priority": self.priority.value,
            "timestamp": self.timestamp.isoformat(),
            "sequence": self.sequence,
        }


class MultiAgentAggregator:
    """Aggregate streams from multiple agents with priority ordering."""

    def __init__(self, run_id: str):
        """Initialize aggregator.

        Args:
            run_id: Associated run ID
        """
        self.run_id = run_id
        self.sequence_counter = 0
        self.agent_streams: dict[str, AsyncIterator] = {}
        self.active_agents: set[str] = set()
        self.event_queue: asyncio.PriorityQueue = asyncio.PriorityQueue()

    def register_agent_stream(
        self,
        agent_id: str,
        stream: AsyncIterator[dict],
    ) -> None:
        """Register agent stream.

        Args:
            agent_id: Agent identifier
            stream: Async iterator of events
        """
        self.agent_streams[agent_id] = stream
        self.active_agents.add(agent_id)
        logger.info(f"Registered agent stream: {agent_id}")

    async def _consume_agent_stream(self, agent_id: str) -> None:
        """Consume events from single agent stream.

        Args:
            agent_id: Agent identifier
        """
        stream = self.agent_streams.get(agent_id)
        if not stream:
            return

        try:
            async for event_dict in stream:
                # Determine priority
                event_type = event_dict.get("event_type", "")
                priority = self._get_event_priority(event_type)

                # Create prioritized event
                event = PrioritizedEvent(
                    event_type=event_type,
                    data=event_dict.get("data", {}),
                    agent_id=agent_id,
                    priority=priority,
                    sequence=self.sequence_counter,
                )
                self.sequence_counter += 1

                # Add to queue (priority, sequence for stable ordering)
                await self.event_queue.put((priority.value, event.sequence, event))

        except Exception as e:
            logger.error(f"Agent stream error ({agent_id}): {e}")
            # Send error event
            event = PrioritizedEvent(
                event_type="agent_error",
                data={"agent_id": agent_id, "error": str(e)},
                agent_id=agent_id,
                priority=EventPriority.CRITICAL,
                sequence=self.sequence_counter,
            )
            self.sequence_counter += 1
            await self.event_queue.put((event.priority.value, event.sequence, event))

        finally:
            self.active_agents.discard(agent_id)
            logger.info(f"Agent stream ended: {agent_id}")

    async def aggregate(self) -> AsyncIterator[dict]:
        """Aggregate all agent streams in priority order.

        Yields:
            Prioritized events from all agents
        """
        # Start consumer tasks for all agents
        tasks = [
            asyncio.create_task(self._consume_agent_stream(agent_id))
            for agent_id in self.agent_streams.keys()
        ]

        try:
            while self.active_agents or not self.event_queue.empty():
                try:
                    # Get next event with timeout
                    priority, sequence, event = await asyncio.wait_for(
                        self.event_queue.get(), timeout=1.0
                    )
                    yield event.to_dict()

                except asyncio.TimeoutError:
                    # Check if all agents are done
                    if not self.active_agents:
                        break
                    # Otherwise continue waiting
                    continue

        finally:
            # Cancel all consumer tasks
            for task in tasks:
                task.cancel()

            # Wait for cancellation
            await asyncio.gather(*tasks, return_exceptions=True)

    def _get_event_priority(self, event_type: str) -> EventPriority:
        """Determine priority for event type.

        Args:
            event_type: Type of event

        Returns:
            Priority level
        """
        critical_types = {"error", "agent_error", "cancelled", "failed"}
        high_types = {"decision", "tool_call", "skill_selected"}

        if event_type in critical_types:
            return EventPriority.CRITICAL
        elif event_type in high_types:
            return EventPriority.HIGH
        elif event_type in {"progress", "intermediate_result"}:
            return EventPriority.NORMAL
        else:
            return EventPriority.LOW

    def get_stats(self) -> dict:
        """Get aggregation statistics.

        Returns:
            Stats dict
        """
        return {
            "run_id": self.run_id,
            "total_events": self.sequence_counter,
            "queued_events": self.event_queue.qsize(),
            "active_agents": len(self.active_agents),
            "registered_agents": len(self.agent_streams),
        }
