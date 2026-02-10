"""Causal chain manager for event tracking.

Manages causal chains and parent-child relationships between events.
"""

from core.events.event_reader import EventReader
from core.events.models import ConversationEvent
from sdk.database import Database


class CausalChainManager:
    """Manager for causal chains.

    Provides methods to track and query event causality.
    """

    def __init__(self, db: Database | None = None) -> None:
        """Initialize causal chain manager.

        Args:
            db: Database instance. If None, creates a new one.
        """
        self.db = db or Database()
        self.reader = EventReader(db)

    def get_chain(self, causal_chain_id: str) -> list[ConversationEvent]:
        """Get all events in a causal chain.

        Args:
            causal_chain_id: Causal chain identifier

        Returns:
            list[ConversationEvent]: Events in chronological order
        """
        return self.reader.get_causal_chain(causal_chain_id)

    def get_parent_event(self, event_id: str) -> ConversationEvent | None:
        """Get the parent event of an event.

        Args:
            event_id: Event identifier

        Returns:
            Optional[ConversationEvent]: Parent event if exists
        """
        event = self.reader.get_event(event_id)
        if not event or not event.parent_event_id:
            return None
        return self.reader.get_event(event.parent_event_id)

    def get_child_events(self, event_id: str) -> list[ConversationEvent]:
        """Get all child events of an event.

        Args:
            event_id: Event identifier

        Returns:
            list[ConversationEvent]: Child events
        """
        query = """
            SELECT * FROM conversation_events
            WHERE parent_event_id = %s
            ORDER BY created_at ASC
        """
        rows = self.db.fetchall(query, (event_id,))
        return [self.reader._row_to_event(row) for row in rows]

    def get_chain_summary(self, causal_chain_id: str) -> dict:
        """Get summary statistics for a causal chain.

        Args:
            causal_chain_id: Causal chain identifier

        Returns:
            dict: Summary with event counts, token usage, etc.
        """
        events = self.get_chain(causal_chain_id)

        total_tokens = 0
        event_types: dict[str, int] = {}

        for event in events:
            # Count event types
            event_type = event.event_type
            event_types[event_type] = event_types.get(event_type, 0) + 1

            # Sum token usage
            if event.token_usage:
                total_tokens += event.token_usage.total

        return {
            "causal_chain_id": causal_chain_id,
            "total_events": len(events),
            "event_types": event_types,
            "total_tokens": total_tokens,
            "first_event_at": events[0].created_at if events else None,
            "last_event_at": events[-1].created_at if events else None,
        }

    def validate_chain_integrity(self, causal_chain_id: str) -> dict:
        """Validate the integrity of a causal chain.

        Checks that parent-child relationships are consistent.

        Args:
            causal_chain_id: Causal chain identifier

        Returns:
            dict: Validation result with is_valid and issues list
        """
        events = self.get_chain(causal_chain_id)
        issues = []

        event_ids = {e.event_id for e in events}

        for event in events:
            # Check parent exists in chain
            if event.parent_event_id and event.parent_event_id not in event_ids:
                issues.append(
                    f"Event {event.event_id} references parent {event.parent_event_id} not in chain"
                )

        return {"is_valid": len(issues) == 0, "issues": issues, "event_count": len(events)}
