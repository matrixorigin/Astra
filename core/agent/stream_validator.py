"""Stream validator for AG-UI protocol compliance.

Validates that stream event sequences follow the AG-UI protocol:
RUN_STARTED → REASONING_*? → TOOL_CALL_START → TOOL_CALL_ARGS →
TOOL_CALL_END → TOOL_RESULT → TEXT_MESSAGE_START → TEXT_MESSAGE_CONTENT* →
TEXT_MESSAGE_END → RUN_FINISHED

Supports both AG-UI standard events and backward-compatible aliases:
- TEXT_DELTA/TEXT_DONE (legacy) map to TEXT_GENERATING state
- TEXT_MESSAGE_START/CONTENT/END (AG-UI) map to TEXT_GENERATING state
- THINKING_DELTA/DONE (deprecated) map to REASONING state
- REASONING_* (AG-UI) map to REASONING state

Edge Cases:
- RUN_ERROR can occur at any point and transitions to ERROR state
- STEP_STARTED/STEP_FINISHED are valid within STARTED state
- STATE_SNAPSHOT/STATE_DELTA/MESSAGES_SNAPSHOT are valid at any active state
- PLAN_* events are valid after RUN_STARTED
- AGENT_* events are valid for multi-agent coordination
- CUSTOM/RAW events are pass-through (no state change)
"""

from collections.abc import AsyncIterator
from enum import Enum

from core.events.models import StreamEvent, StreamEventType
from core.logging_config import get_logger

logger = get_logger(__name__)

# Events that are valid in any active (non-terminal) state
_PASSTHROUGH_EVENTS = frozenset({
    StreamEventType.STATE_SNAPSHOT,
    StreamEventType.STATE_DELTA,
    StreamEventType.MESSAGES_SNAPSHOT,
    StreamEventType.CUSTOM,
    StreamEventType.RAW,
    StreamEventType.STEP_STARTED,
    StreamEventType.STEP_FINISHED,
})


class StreamState(str, Enum):
    """Valid states in the AG-UI protocol state machine."""

    IDLE = "idle"
    STARTED = "started"
    REASONING = "reasoning"
    TOOL_CALLING = "tool_calling"
    TOOL_EXECUTING = "tool_executing"
    TEXT_GENERATING = "text_generating"
    FINISHED = "finished"
    ERROR = "error"

    # Backward-compatible alias
    THINKING = "reasoning"


class StreamValidator:
    """Validate stream event sequences against AG-UI protocol."""

    def __init__(self):
        self.state = StreamState.IDLE
        self.violations = []
        self.event_count = 0

    async def validate_stream(
        self, stream: AsyncIterator[StreamEvent]
    ) -> AsyncIterator[StreamEvent]:
        """Validate stream while passing through events."""
        async for event in stream:
            self._validate_transition(event)
            self.event_count += 1
            yield event

        if self.state not in (StreamState.FINISHED, StreamState.ERROR):
            self._add_violation(f"Stream ended in unexpected state: {self.state}")

    def _validate_transition(self, event: StreamEvent):
        """Validate state transition for an event."""
        event_type = event.event_type

        # RUN_ERROR can occur at any point
        if event_type == StreamEventType.RUN_ERROR:
            self._update_state(event_type)
            return

        # Passthrough events: valid in any active state, no state change
        if event_type in _PASSTHROUGH_EVENTS:
            if self.state in (StreamState.IDLE, StreamState.FINISHED, StreamState.ERROR):
                self._add_violation(
                    f"Passthrough event {event_type} in terminal state: {self.state}"
                )
            return

        valid_transitions = {
            StreamState.IDLE: [StreamEventType.RUN_STARTED],
            StreamState.STARTED: [
                # Legacy
                StreamEventType.THINKING_DELTA,
                StreamEventType.TEXT_DELTA,
                # AG-UI standard
                StreamEventType.REASONING_START,
                StreamEventType.REASONING_MESSAGE_START,
                StreamEventType.REASONING_MESSAGE_CONTENT,
                StreamEventType.TEXT_MESSAGE_START,
                StreamEventType.TEXT_MESSAGE_CONTENT,
                # Common
                StreamEventType.TOOL_CALL_START,
                StreamEventType.PLAN_CREATED,
                StreamEventType.AGENT_DELEGATED,
                StreamEventType.RUN_FINISHED,
            ],
            StreamState.REASONING: [
                # Legacy
                StreamEventType.THINKING_DELTA,
                StreamEventType.THINKING_DONE,
                # AG-UI standard
                StreamEventType.REASONING_MESSAGE_CONTENT,
                StreamEventType.REASONING_MESSAGE_END,
                StreamEventType.REASONING_END,
                # Transitions out
                StreamEventType.TOOL_CALL_START,
                StreamEventType.TEXT_DELTA,
                StreamEventType.TEXT_MESSAGE_START,
            ],
            StreamState.TOOL_CALLING: [
                StreamEventType.TOOL_CALL_ARGS,
                StreamEventType.TOOL_CALL_END,
            ],
            StreamState.TOOL_EXECUTING: [
                StreamEventType.TOOL_RESULT,
                StreamEventType.TOOL_CALL_START,
                StreamEventType.TEXT_DELTA,
                StreamEventType.TEXT_MESSAGE_START,
                StreamEventType.RUN_FINISHED,
            ],
            StreamState.TEXT_GENERATING: [
                # Legacy
                StreamEventType.TEXT_DELTA,
                StreamEventType.TEXT_DONE,
                # AG-UI standard
                StreamEventType.TEXT_MESSAGE_CONTENT,
                StreamEventType.TEXT_MESSAGE_END,
                # Common
                StreamEventType.RUN_FINISHED,
            ],
            StreamState.FINISHED: [],
            StreamState.ERROR: [],
        }

        allowed = valid_transitions.get(self.state, [])
        if event_type not in allowed:
            self._add_violation(
                f"Invalid transition: {self.state} -> {event_type} "
                f"(allowed: {[t.value for t in allowed]})"
            )

        self._update_state(event_type)

    def _update_state(self, event_type: StreamEventType):
        """Update state machine based on event type."""
        _state_map = {
            # Lifecycle
            StreamEventType.RUN_STARTED: StreamState.STARTED,
            StreamEventType.RUN_FINISHED: StreamState.FINISHED,
            StreamEventType.RUN_ERROR: StreamState.ERROR,
            # Reasoning (AG-UI)
            StreamEventType.REASONING_START: StreamState.REASONING,
            StreamEventType.REASONING_MESSAGE_START: StreamState.REASONING,
            StreamEventType.REASONING_MESSAGE_CONTENT: StreamState.REASONING,
            StreamEventType.REASONING_MESSAGE_END: StreamState.STARTED,
            StreamEventType.REASONING_END: StreamState.STARTED,
            # Thinking (legacy → reasoning)
            StreamEventType.THINKING_DELTA: StreamState.REASONING,
            StreamEventType.THINKING_DONE: StreamState.STARTED,
            # Tool use
            StreamEventType.TOOL_CALL_START: StreamState.TOOL_CALLING,
            StreamEventType.TOOL_CALL_END: StreamState.TOOL_EXECUTING,
            StreamEventType.TOOL_RESULT: StreamState.STARTED,
            # Text (AG-UI)
            StreamEventType.TEXT_MESSAGE_START: StreamState.TEXT_GENERATING,
            StreamEventType.TEXT_MESSAGE_CONTENT: StreamState.TEXT_GENERATING,
            StreamEventType.TEXT_MESSAGE_END: StreamState.STARTED,
            # Text (legacy)
            StreamEventType.TEXT_DELTA: StreamState.TEXT_GENERATING,
            StreamEventType.TEXT_DONE: StreamState.STARTED,
        }
        new_state = _state_map.get(event_type)
        if new_state:
            self.state = new_state

    def _add_violation(self, message: str):
        """Record a protocol violation."""
        violation = {
            "event_number": self.event_count,
            "state": self.state.value,
            "message": message,
        }
        self.violations.append(violation)
        logger.warning(f"Stream protocol violation: {message}")

    def get_report(self) -> dict:
        """Get validation report."""
        return {
            "total_events": self.event_count,
            "final_state": self.state.value,
            "violations": self.violations,
            "is_valid": len(self.violations) == 0,
        }


async def validate_stream(stream: AsyncIterator[StreamEvent]) -> AsyncIterator[StreamEvent]:
    """Convenience: validate a stream with a fresh validator."""
    validator = StreamValidator()
    async for event in validator.validate_stream(stream):
        yield event
