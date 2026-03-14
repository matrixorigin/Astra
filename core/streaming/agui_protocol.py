"""AG-UI protocol compliance validator.

Ensure streamed events conform to AG-UI protocol specification.
"""

from __future__ import annotations

import logging
from dataclasses import dataclass
from enum import Enum
from typing import Optional

logger = logging.getLogger(__name__)


class EventTypeCategory(str, Enum):
    """AG-UI event type categories."""

    SESSION = "session"
    PROGRESS = "progress"
    DECISION = "decision"
    TOOL = "tool"
    RESULT = "result"
    ERROR = "error"
    METADATA = "metadata"


@dataclass
class EventSchema:
    """Schema for event type."""

    event_type: str
    category: EventTypeCategory
    required_fields: list[str]
    optional_fields: list[str]
    description: str


class AGUIProtocolValidator:
    """Validate events against AG-UI protocol."""

    # AG-UI event schemas
    SCHEMAS = {
        "session_info": EventSchema(
            event_type="session_info",
            category=EventTypeCategory.SESSION,
            required_fields=["session_id", "run_id"],
            optional_fields=["user_id", "agent_id"],
            description="Session initialization",
        ),
        "run_started": EventSchema(
            event_type="run_started",
            category=EventTypeCategory.PROGRESS,
            required_fields=["run_id"],
            optional_fields=["timestamp"],
            description="Run execution started",
        ),
        "progress": EventSchema(
            event_type="progress",
            category=EventTypeCategory.PROGRESS,
            required_fields=["run_id", "status"],
            optional_fields=["percentage", "message"],
            description="Execution progress update",
        ),
        "decision": EventSchema(
            event_type="decision",
            category=EventTypeCategory.DECISION,
            required_fields=["run_id", "decision_type", "decision"],
            optional_fields=["reasoning", "confidence"],
            description="Agent decision point",
        ),
        "tool_call": EventSchema(
            event_type="tool_call",
            category=EventTypeCategory.TOOL,
            required_fields=["run_id", "tool_name", "arguments"],
            optional_fields=["tool_id"],
            description="Tool invocation",
        ),
        "tool_result": EventSchema(
            event_type="tool_result",
            category=EventTypeCategory.TOOL,
            required_fields=["run_id", "tool_name", "result"],
            optional_fields=["tool_id", "execution_time_ms"],
            description="Tool execution result",
        ),
        "result": EventSchema(
            event_type="result",
            category=EventTypeCategory.RESULT,
            required_fields=["run_id", "content"],
            optional_fields=["metadata"],
            description="Final result",
        ),
        "error": EventSchema(
            event_type="error",
            category=EventTypeCategory.ERROR,
            required_fields=["run_id", "error"],
            optional_fields=["error_code", "details"],
            description="Error occurred",
        ),
        "run_completed": EventSchema(
            event_type="run_completed",
            category=EventTypeCategory.PROGRESS,
            required_fields=["run_id", "status"],
            optional_fields=["duration_ms"],
            description="Run execution completed",
        ),
        "heartbeat": EventSchema(
            event_type="heartbeat",
            category=EventTypeCategory.METADATA,
            required_fields=["timestamp"],
            optional_fields=["run_id"],
            description="Connection keepalive",
        ),
    }

    def __init__(self):
        """Initialize validator."""
        self.validation_errors: list[str] = []
        self.validation_warnings: list[str] = []

    def validate_event(self, event: dict) -> bool:
        """Validate event against schema.

        Args:
            event: Event dict to validate

        Returns:
            True if valid, False otherwise
        """
        self.validation_errors.clear()
        self.validation_warnings.clear()

        event_type = event.get("event_type")
        if not event_type:
            self.validation_errors.append("Missing event_type")
            return False

        schema = self.SCHEMAS.get(event_type)
        if not schema:
            self.validation_warnings.append(f"Unknown event_type: {event_type}")
            return True  # Allow unknown types for extensibility

        # Check required fields
        data = event.get("data", {})
        for field in schema.required_fields:
            if field not in data:
                self.validation_errors.append(f"Missing required field '{field}' in {event_type}")

        # Check for unexpected fields
        allowed_fields = set(schema.required_fields) | set(schema.optional_fields)
        for field in data.keys():
            if field not in allowed_fields:
                self.validation_warnings.append(f"Unexpected field '{field}' in {event_type}")

        return len(self.validation_errors) == 0

    def validate_stream(self, events: list[dict]) -> dict:
        """Validate entire event stream.

        Args:
            events: List of events

        Returns:
            Validation report
        """
        report = {
            "total_events": len(events),
            "valid_events": 0,
            "invalid_events": 0,
            "errors": [],
            "warnings": [],
            "event_type_distribution": {},
        }

        for event in events:
            event_type = event.get("event_type", "unknown")
            report["event_type_distribution"][event_type] = (
                report["event_type_distribution"].get(event_type, 0) + 1
            )

            if self.validate_event(event):
                report["valid_events"] += 1
            else:
                report["invalid_events"] += 1
                report["errors"].extend(self.validation_errors)

            report["warnings"].extend(self.validation_warnings)

        return report

    def get_schema(self, event_type: str) -> Optional[EventSchema]:
        """Get schema for event type.

        Args:
            event_type: Event type

        Returns:
            Schema or None
        """
        return self.SCHEMAS.get(event_type)

    def list_event_types(self) -> list[str]:
        """List all supported event types.

        Returns:
            List of event types
        """
        return list(self.SCHEMAS.keys())
