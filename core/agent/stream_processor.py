"""Unified stream processing facade for common patterns.

Provides a simple API for common stream processing workflows while
maintaining the flexibility of the underlying modular components.
"""

from collections.abc import AsyncIterator
from datetime import datetime

from sqlalchemy.orm import Session

from core.agent.stream_validator import StreamValidator
from core.agent.stream_persistence import StreamPersistence
from core.agent.stream_replay import StreamReplay
from core.events.event_logger import EventLogger
from core.events.models import StreamEvent
from core.db_consumer import DbConsumer, DbFactory


class StreamProcessor(DbConsumer):
    """Facade for common stream processing patterns.
    
    Combines StreamValidator, StreamPersistence, and StreamReplay
    into a unified interface for common use cases.
    """
    
    def __init__(self, db_factory: DbFactory):
        """Initialize stream processor.
        
        Args:
            db: Database session
        """
        super().__init__(db_factory)
        self.event_logger = EventLogger(db_factory)
        self.validator = StreamValidator()
        self.persistence = StreamPersistence(self.event_logger)
        self.replay = StreamReplay(db_factory)
    
    async def process_and_persist(
        self,
        stream: AsyncIterator[StreamEvent],
        user_id: str,
        session_id: str,
        agent_id: str,
        agent_version: str,
        causal_chain_id: str,
        validate: bool = True,
    ) -> AsyncIterator[StreamEvent]:
        """Process stream with validation and persistence.
        
        Common pattern: validate AG-UI protocol compliance and persist
        events to database while streaming to client.
        
        Args:
            stream: Input stream iterator
            user_id: User identifier
            session_id: Session identifier
            agent_id: Agent identifier
            agent_version: Agent version
            causal_chain_id: Causal chain ID for linking
            validate: Enable AG-UI protocol validation (default: True)
            
        Yields:
            StreamEvent: Validated and persisted events
        """
        if validate:
            stream = self.validator.validate_stream(stream)
        
        async for event in self.persistence.persist_stream(
            stream=stream,
            user_id=user_id,
            session_id=session_id,
            agent_id=agent_id,
            agent_version=agent_version,
            causal_chain_id=causal_chain_id,
        ):
            yield event
    
    async def replay_stream(
        self,
        session_id: str,
        causal_chain_id: str | None = None,
    ) -> AsyncIterator[StreamEvent]:
        """Replay stream from database.
        
        Args:
            session_id: Session to replay
            causal_chain_id: Optional causal chain filter
            
        Yields:
            StreamEvent: Reconstructed stream events
        """
        async for event in self.replay.replay_stream(session_id, causal_chain_id):
            yield event
    
    async def replay_stream_at(
        self,
        session_id: str,
        timestamp: datetime,
        causal_chain_id: str | None = None,
    ) -> AsyncIterator[StreamEvent]:
        """Replay stream up to a specific timestamp (time-travel).
        
        Args:
            session_id: Session to replay
            timestamp: Replay up to this point in time
            causal_chain_id: Optional causal chain filter
            
        Yields:
            StreamEvent: Reconstructed stream events up to timestamp
        """
        async for event in self.replay.replay_stream_at(
            session_id, timestamp, causal_chain_id
        ):
            yield event
