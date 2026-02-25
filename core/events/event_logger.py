"""Event logger for writing conversation events.

Handles event creation and persistence to the database.
When EVENT_PIPELINE_ENABLED=true (default), delegates writes to EventPipeline
for async batched ingestion. Otherwise falls back to synchronous DB writes.
"""

import json
import os
from typing import Callable

from sqlalchemy.orm import Session
from uuid_utils import uuid7

from api.models import Event as EventModel
from core.db_consumer import DbConsumer
from core.events.models import ConversationEvent, EventType

_PIPELINE_ENABLED = os.environ.get("EVENT_PIPELINE_ENABLED", "true").lower() in ("true", "1", "yes")


class EventLogger(DbConsumer):
    """Logger for conversation events.

    Provides methods to create and persist events following the event-centric design.
    When a pipeline is attached, log_event() delegates to pipeline.emit() (async).
    """

    def __init__(self, db_factory: Callable[[], Session], pipeline=None) -> None:
        """Initialize event logger.

        Args:
            db_factory: Callable that returns a new SQLAlchemy Session.
                        For legacy callers that still hold a raw Session,
                        use ``EventLogger.from_session(db)`` instead.
            pipeline: Optional EventPipeline for async writes.
        """
        super().__init__(db_factory)
        self._pipeline = pipeline

    @classmethod
    def from_session(cls, session: Session, pipeline=None) -> "EventLogger":
        """Create an EventLogger backed by an existing session.

        The session will NOT be closed by ``_db()`` — ownership stays with
        the caller.  Use this for request-scoped sessions that outlive the
        EventLogger.

        Raises TypeError if *session* is not a SQLAlchemy Session.
        """
        if not isinstance(session, Session):
            raise TypeError(f"session must be a SQLAlchemy Session, got {type(session).__name__}")
        inst = cls(lambda: session, pipeline=pipeline)

        # Override _db to yield the borrowed session without closing it.
        from contextlib import contextmanager

        @contextmanager
        def _borrowed_db():
            try:
                yield session
            except Exception:
                session.rollback()
                raise

        inst._db = _borrowed_db  # type: ignore[assignment]
        inst._borrowed_session = session  # expose for introspection/tests
        return inst

    def log_event(self, event: ConversationEvent) -> str:
        """Log a conversation event to the database.

        When pipeline is attached and enabled, delegates to pipeline.emit().
        Otherwise falls back to synchronous DB write.

        Args:
            event: Event to log

        Returns:
            str: Event ID
        """
        # Async path: delegate to pipeline
        if self._pipeline and _PIPELINE_ENABLED:
            return self._pipeline.emit(event)

        # Synchronous path (legacy)
        # Embedding is now decoupled — generated asynchronously by EmbeddingWorker
        # Extract high-frequency query fields from metadata
        metadata = event.metadata or {}
        run_id = metadata.get('run_id')
        parent_run_id = metadata.get('parent_run_id')
        waiting_for = metadata.get('waiting_for')
        
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
            embedding=None,  # No longer written inline; EmbeddingWorker fills event_embeddings
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
            # High-frequency query fields
            run_id=run_id,
            parent_run_id=parent_run_id,
            waiting_for=waiting_for,
        )
        
        with self._db() as db:
            db.add(db_event)
            db.commit()
        return event.event_id

    def flush_critical(self) -> None:
        """Flush critical events synchronously via pipeline.

        No-op when pipeline is not attached (synchronous writes already committed).
        """
        if self._pipeline and _PIPELINE_ENABLED:
            self._pipeline.flush_critical()

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

    def update_quality_score(
        self, event_id: str, quality_score: float, training_eligible: bool,
    ) -> None:
        """Update quality_score and training_eligible on an existing event."""
        with self._db() as db:
            db.query(EventModel).filter(
                EventModel.event_id == event_id,
            ).update(
                {"quality_score": quality_score, "training_eligible": training_eligible},
            )
            db.commit()
