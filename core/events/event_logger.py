"""Event logger for writing conversation events.

Handles event creation and persistence to the database.
"""

import json

from uuid_utils import uuid7

from core.events.models import ConversationEvent, EventType
from sdk.database import Database


class EventLogger:
    """Logger for conversation events.

    Provides methods to create and persist events following the event-centric design.
    """

    def __init__(self, db: Database | None = None) -> None:
        """Initialize event logger.

        Args:
            db: Database instance. If None, creates a new one.
        """
        self.db = db or Database()

    def log_event(self, event: ConversationEvent) -> str:
        """Log a conversation event to the database.

        Args:
            event: Event to log

        Returns:
            str: Event ID

        Raises:
            Exception: If database operation fails
        """
        query = """
            INSERT INTO conversation_events (
                event_id, user_id, session_id, agent_id, agent_version,
                event_type, content, desensitized_content, metadata,
                context_snapshot, token_usage, embedding_ref, created_at,
                prompt_template_id, skills_snapshot, quality_score,
                is_flagged, training_eligible, parent_event_id,
                causal_chain_id, llm_model_used, llm_params
            ) VALUES (
                %s, %s, %s, %s, %s, %s, %s, %s, %s, %s, %s, %s, %s,
                %s, %s, %s, %s, %s, %s, %s, %s, %s
            )
        """

        params = (
            event.event_id,
            event.user_id,
            event.session_id,
            event.agent_id,
            event.agent_version,
            event.event_type,
            event.content,
            event.desensitized_content,
            json.dumps(event.metadata) if event.metadata else None,
            (event.context_snapshot.model_dump_json() if event.context_snapshot else None),
            event.token_usage.model_dump_json() if event.token_usage else None,
            event.embedding_ref,
            event.created_at,
            event.prompt_template_id,
            json.dumps(event.skills_snapshot) if event.skills_snapshot else None,
            event.quality_score,
            event.is_flagged,
            event.training_eligible,
            event.parent_event_id,
            event.causal_chain_id,
            event.llm_model_used,
            json.dumps(event.llm_params) if event.llm_params else None,
        )

        self.db.execute(query, params)
        return event.event_id

    def create_stream_event(
        self,
        user_id: str,
        session_id: str,
        event_type: str,
        content: str,
        agent_id: str = "dev-agent",
        agent_version: str = "0.1.0",
        parent_event_id: str | None = None,
        causal_chain_id: str | None = None,
        metadata: dict | None = None,
    ) -> ConversationEvent:
        """Create and log a stream event to the database.

        Args:
            user_id: User identifier
            session_id: Session identifier
            event_type: Stream event type (e.g., stream_text_delta)
            content: Event content (JSON string)
            agent_id: Agent identifier
            agent_version: Agent version
            parent_event_id: Parent event ID in causal chain
            causal_chain_id: Causal chain identifier
            metadata: Additional metadata

        Returns:
            ConversationEvent: Created event
        """
        # Map stream event types to valid EventType enum values
        # Use SYSTEM_MESSAGE for streaming events since they're system-generated
        event_type_map = {
            "stream_run_started": EventType.SYSTEM_MESSAGE,
            "stream_run_finished": EventType.SYSTEM_MESSAGE,
            "stream_run_error": EventType.SYSTEM_MESSAGE,
            "stream_text_delta": EventType.SYSTEM_MESSAGE,
            "stream_text_done": EventType.SYSTEM_MESSAGE,
            "stream_thinking_delta": EventType.SYSTEM_MESSAGE,
            "stream_thinking_done": EventType.SYSTEM_MESSAGE,
            "stream_tool_call_start": EventType.SYSTEM_MESSAGE,
            "stream_tool_call_args": EventType.SYSTEM_MESSAGE,
            "stream_tool_call_end": EventType.SYSTEM_MESSAGE,
            "stream_tool_result": EventType.SYSTEM_MESSAGE,
            "stream_plan_created": EventType.SYSTEM_MESSAGE,
            "stream_plan_step_start": EventType.SYSTEM_MESSAGE,
            "stream_plan_step_done": EventType.SYSTEM_MESSAGE,
            "stream_plan_revised": EventType.SYSTEM_MESSAGE,
            "stream_agent_delegated": EventType.SYSTEM_MESSAGE,
            "stream_agent_progress": EventType.SYSTEM_MESSAGE,
            "stream_agent_completed": EventType.SYSTEM_MESSAGE,
        }
        
        # Map to valid EventType, default to SYSTEM_MESSAGE
        mapped_event_type = event_type_map.get(event_type, EventType.SYSTEM_MESSAGE)
        
        event = ConversationEvent(
            event_id=str(uuid7()),
            user_id=user_id,
            session_id=session_id,
            agent_id=agent_id,
            agent_version=agent_version,
            event_type=mapped_event_type,
            content=content,
            parent_event_id=parent_event_id,
            causal_chain_id=causal_chain_id or str(uuid7()),
            metadata=metadata or {"stream_event_type": event_type},
        )
        self.log_event(event)
        return event

    def create_user_query(
        self,
        user_id: str,
        session_id: str,
        content: str,
        agent_id: str = "dev-agent",
        agent_version: str = "0.1.0",
        parent_event_id: str | None = None,
        causal_chain_id: str | None = None,
    ) -> ConversationEvent:
        """Create and log a user query event.

        Args:
            user_id: User identifier
            session_id: Session identifier
            content: User query content
            agent_id: Agent identifier
            agent_version: Agent version
            parent_event_id: Parent event ID in causal chain
            causal_chain_id: Causal chain identifier

        Returns:
            ConversationEvent: Created event
        """
        event = ConversationEvent(
            event_id=str(uuid7()),
            user_id=user_id,
            session_id=session_id,
            agent_id=agent_id,
            agent_version=agent_version,
            event_type=EventType.USER_QUERY,
            content=content,
            parent_event_id=parent_event_id,
            causal_chain_id=causal_chain_id or str(uuid7()),
        )
        self.log_event(event)
        return event

    def create_llm_response(
        self,
        user_id: str,
        session_id: str,
        content: str,
        agent_id: str,
        agent_version: str,
        parent_event_id: str,
        causal_chain_id: str,
        llm_model_used: str | None = None,
        token_usage: dict | None = None,
        llm_params: dict | None = None,
    ) -> ConversationEvent:
        """Create and log an LLM response event.

        Args:
            user_id: User identifier
            session_id: Session identifier
            content: LLM response content
            agent_id: Agent identifier
            agent_version: Agent version
            parent_event_id: Parent event ID (usually the LLM request)
            causal_chain_id: Causal chain identifier
            llm_model_used: LLM model identifier
            token_usage: Token usage dict with prompt, completion, total
            llm_params: LLM parameters

        Returns:
            ConversationEvent: Created event
        """
        from core.events.models import TokenUsage

        event = ConversationEvent(
            event_id=str(uuid7()),
            user_id=user_id,
            session_id=session_id,
            agent_id=agent_id,
            agent_version=agent_version,
            event_type=EventType.LLM_RESPONSE,
            content=content,
            parent_event_id=parent_event_id,
            causal_chain_id=causal_chain_id,
            llm_model_used=llm_model_used,
            token_usage=TokenUsage(**token_usage) if token_usage else None,
            llm_params=llm_params,
        )
        self.log_event(event)
        return event
