"""Event logger for writing conversation events.

Handles event creation and persistence to the database.
"""

import json

from sqlalchemy.orm import Session
from uuid_utils import uuid7

from api.database import get_db_session
from api.models import Event as EventModel
from core.events.models import ConversationEvent, EventType


class EventLogger:
    """Logger for conversation events.

    Provides methods to create and persist events following the event-centric design.
    """

    def __init__(self, session: Session | None = None) -> None:
        """Initialize event logger.

        Args:
            session: SQLAlchemy session. If None, creates a new one.
        """
        self._session = session
        self._owns_session = session is None

    def _get_session(self) -> Session:
        """Get or create session."""
        if self._session is None:
            self._session = next(get_db_session())
        return self._session

    def __del__(self):
        """Close session if we own it."""
        if self._owns_session and self._session:
            self._session.close()

    def log_event(self, event: ConversationEvent) -> str:
        """Log a conversation event to the database.

        Args:
            event: Event to log

        Returns:
            str: Event ID

        Raises:
            Exception: If database operation fails
        """
        session = self._get_session()
        
        db_event = EventModel(
            event_id=event.event_id,
            user_id=event.user_id,
            session_id=event.session_id,
            agent_id=event.agent_id,
            agent_version=event.agent_version,
            event_type=event.event_type,
            content=event.content,
            desensitized_content=event.desensitized_content,
            event_metadata=event.metadata,
            context_snapshot=event.context_snapshot.model_dump() if event.context_snapshot else None,
            token_usage=event.token_usage.model_dump() if event.token_usage else None,
            embedding_ref=event.embedding_ref,
            created_at=event.created_at,
            prompt_template_id=event.prompt_template_id,
            skills_snapshot=event.skills_snapshot,
            quality_score=event.quality_score,
            is_flagged=event.is_flagged,
            training_eligible=event.training_eligible,
            parent_event_id=event.parent_event_id,
            causal_chain_id=event.causal_chain_id,
            llm_model_used=event.llm_model_used,
            llm_params=event.llm_params,
        )
        
        session.add(db_event)
        session.commit()
        return event.event_id

    def create_plan_event(
        self,
        user_id: str,
        session_id: str,
        event_type: str,
        plan_data: dict,
        agent_id: str = "dev-agent",
        agent_version: str = "0.1.0",
        parent_event_id: str | None = None,
        causal_chain_id: str | None = None,
        metadata: dict | None = None,
    ) -> ConversationEvent:
        """Create and log a plan event.

        Args:
            user_id: User identifier
            session_id: Session identifier
            event_type: Plan event type (plan_created, plan_revised, etc.)
            plan_data: Plan data dictionary
            agent_id: Agent identifier
            agent_version: Agent version
            parent_event_id: Parent event ID
            causal_chain_id: Causal chain ID
            metadata: Additional metadata

        Returns:
            ConversationEvent: Created event
        """
        event_id = str(uuid7())
        chain_id = causal_chain_id or str(uuid7())

        # Merge plan_data into metadata
        event_metadata = metadata or {}
        if "plan_id" in plan_data:
            event_metadata["plan_id"] = plan_data["plan_id"]
        if "goal" in plan_data:
            event_metadata["goal"] = plan_data["goal"]
        if "revision_of" in plan_data and plan_data["revision_of"]:
            event_metadata["revision_of"] = plan_data["revision_of"]

        event = ConversationEvent(
            event_id=event_id,
            user_id=user_id,
            session_id=session_id,
            agent_id=agent_id,
            agent_version=agent_version,
            event_type=event_type,
            content=json.dumps(plan_data),
            parent_event_id=parent_event_id,
            causal_chain_id=chain_id,
            metadata=event_metadata,
        )

        self.log_event(event)
        return event

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
        # Map string event_type to EventType enum directly
        try:
            mapped_event_type = EventType(event_type)
        except ValueError:
            mapped_event_type = EventType.SYSTEM_MESSAGE

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
            metadata=metadata,
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
        llm_request_id: str | None = None,
        llm_response_id: str | None = None,
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
            llm_request_id: LLM provider request ID
            llm_response_id: LLM provider response ID

        Returns:
            ConversationEvent: Created event
        """
        from core.events.models import TokenUsage

        # Add IDs to params if provided
        if llm_request_id or llm_response_id:
            llm_params = llm_params or {}
            if llm_request_id:
                llm_params["request_id"] = llm_request_id
            if llm_response_id:
                llm_params["response_id"] = llm_response_id

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
