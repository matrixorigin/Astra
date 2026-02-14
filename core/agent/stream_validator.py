"""Stream validator for AG-UI protocol compliance.

Validates that stream event sequences follow the AG-UI protocol:
RUN_STARTED → THINKING_DELTA* → TOOL_CALL_START → TOOL_CALL_ARGS → 
TOOL_CALL_END → TOOL_RESULT → TEXT_DELTA* → TEXT_DONE → RUN_FINISHED

Edge Cases:
- RUN_ERROR can occur at any point and transitions to ERROR state
- PLAN_* events are valid after RUN_STARTED
- AGENT_* events are valid for multi-agent coordination
"""

from collections.abc import AsyncIterator
from enum import Enum

from core.events.models import StreamEvent, StreamEventType
from core.logging_config import get_logger

logger = get_logger(__name__)


class StreamState(str, Enum):
    """Valid states in the AG-UI protocol state machine."""

    IDLE = "idle"
    STARTED = "started"
    THINKING = "thinking"
    TOOL_CALLING = "tool_calling"
    TOOL_EXECUTING = "tool_executing"
    TEXT_GENERATING = "text_generating"
    FINISHED = "finished"
    ERROR = "error"


class StreamValidator:
    """Validate stream event sequences against AG-UI protocol."""

    def __init__(self):
        self.state = StreamState.IDLE
        self.violations = []
        self.event_count = 0

    async def validate_stream(
        self, stream: AsyncIterator[StreamEvent]
    ) -> AsyncIterator[StreamEvent]:
        """Validate stream while passing through events.

        Args:
            stream: Input stream to validate

        Yields:
            StreamEvent: Same events, with validation side effects
        """
        async for event in stream:
            self._validate_transition(event)
            self.event_count += 1
            yield event

        # Final validation
        if self.state not in (StreamState.FINISHED, StreamState.ERROR):
            self._add_violation(f"Stream ended in unexpected state: {self.state}")

    def _validate_transition(self, event: StreamEvent):
        """Validate state transition for an event.

        Args:
            event: StreamEvent to validate
        """
        event_type = event.event_type
        current_state = self.state

        # RUN_ERROR can occur at any point
        if event_type == StreamEventType.RUN_ERROR:
            self._update_state(event_type)
            return

        # Define valid transitions
        valid_transitions = {
            StreamState.IDLE: [StreamEventType.RUN_STARTED],
            StreamState.STARTED: [
                StreamEventType.THINKING_DELTA,
                StreamEventType.TOOL_CALL_START,
                StreamEventType.TEXT_DELTA,
                StreamEventType.PLAN_CREATED,
                StreamEventType.AGENT_DELEGATED,
                StreamEventType.RUN_FINISHED,
            ],
            StreamState.THINKING: [
                StreamEventType.THINKING_DELTA,
                StreamEventType.THINKING_DONE,
                StreamEventType.TOOL_CALL_START,
                StreamEventType.TEXT_DELTA,
            ],
            StreamState.TOOL_CALLING: [
                StreamEventType.TOOL_CALL_ARGS,
                StreamEventType.TOOL_CALL_END,
            ],
            StreamState.TOOL_EXECUTING: [
                StreamEventType.TOOL_RESULT,
                StreamEventType.TOOL_CALL_START,
                StreamEventType.TEXT_DELTA,
                StreamEventType.RUN_FINISHED,
            ],
            StreamState.TEXT_GENERATING: [
                StreamEventType.TEXT_DELTA,
                StreamEventType.TEXT_DONE,
                StreamEventType.RUN_FINISHED,
            ],
            StreamState.FINISHED: [],
            StreamState.ERROR: [],
        }

        # Check if transition is valid
        allowed = valid_transitions.get(current_state, [])
        if event_type not in allowed:
            self._add_violation(
                f"Invalid transition: {current_state} -> {event_type} "
                f"(allowed: {[t.value for t in allowed]})"
            )

        # Update state
        self._update_state(event_type)

    def _update_state(self, event_type: StreamEventType):
        """Update state machine based on event type.

        Args:
            event_type: StreamEventType that occurred
        """
        if event_type == StreamEventType.RUN_STARTED:
            self.state = StreamState.STARTED
        elif event_type == StreamEventType.THINKING_DELTA:
            self.state = StreamState.THINKING
        elif event_type == StreamEventType.THINKING_DONE:
            self.state = StreamState.STARTED
        elif event_type == StreamEventType.TOOL_CALL_START:
            self.state = StreamState.TOOL_CALLING
        elif event_type == StreamEventType.TOOL_CALL_END:
            self.state = StreamState.TOOL_EXECUTING
        elif event_type == StreamEventType.TOOL_RESULT:
            self.state = StreamState.STARTED
        elif event_type == StreamEventType.TEXT_DELTA:
            self.state = StreamState.TEXT_GENERATING
        elif event_type == StreamEventType.TEXT_DONE:
            self.state = StreamState.STARTED
        elif event_type == StreamEventType.RUN_FINISHED:
            self.state = StreamState.FINISHED
        elif event_type == StreamEventType.RUN_ERROR:
            self.state = StreamState.ERROR

    def _add_violation(self, message: str):
        """Record a protocol violation.

        Args:
            message: Violation description
        """
        violation = {
            "event_number": self.event_count,
            "state": self.state.value,
            "message": message,
        }
        self.violations.append(violation)
        logger.warning(f"Stream protocol violation: {message}")

    def get_report(self) -> dict:
        """Get validation report.

        Returns:
            dict: Validation summary
        """
        return {
            "total_events": self.event_count,
            "final_state": self.state.value,
            "violations": self.violations,
            "is_valid": len(self.violations) == 0,
        }


async def validate_stream(stream: AsyncIterator[StreamEvent]) -> AsyncIterator[StreamEvent]:
    """Convenience function to validate a stream.

    Args:
        stream: Stream to validate

    Yields:
        StreamEvent: Same events, with validation logging
    """
    validator = StreamValidator()
    async for event in validator.validate_stream(stream):
        yield event

    # Log final report
    report = validator.get_report()
    if not report["is_valid"]:
        logger.error(f"Stream validation failed: {len(report['violations'])} violations")
        for v in report["violations"]:
            logger.error(f"  Event {v['event_number']}: {v['message']}")
    else:
        logger.info(f"Stream validation passed: {report['total_events']} events")
