"""Streaming protocol layer — P3 Streaming Protocol.

WebSocket transport + multi-agent aggregation + AG-UI protocol compliance.
"""

from core.streaming.agui_protocol import AGUIProtocolValidator, EventSchema, EventTypeCategory
from core.streaming.multi_agent_aggregator import (
    EventPriority,
    MultiAgentAggregator,
    PrioritizedEvent,
)
from core.streaming.websocket_transport import ConnectionState, StreamEvent, WebSocketTransport

__all__ = [
    # WebSocket transport
    "WebSocketTransport",
    "StreamEvent",
    "ConnectionState",
    # Multi-agent aggregation
    "MultiAgentAggregator",
    "PrioritizedEvent",
    "EventPriority",
    # AG-UI protocol
    "AGUIProtocolValidator",
    "EventSchema",
    "EventTypeCategory",
]
