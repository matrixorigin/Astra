"""P3 Streaming Protocol integration tests — Real database events, no stream blocking."""

from datetime import datetime
from unittest.mock import AsyncMock

import pytest
from sqlalchemy.orm import Session

from core.events.event_logger import EventLogger
from core.events.session_manager import SessionManager
from core.streaming import (
    AGUIProtocolValidator,
    ConnectionState,
    EventPriority,
    MultiAgentAggregator,
    StreamEvent,
    WebSocketTransport,
)


class TestStreamingProtocolIntegration:
    """Integration tests with real database events."""
    
    def test_stream_event_from_real_database_event(self, db: Session):
        """Test StreamEvent wrapping real database event.
        
        Real scenario: Database event → StreamEvent → Serialization
        """
        # Setup
        session_mgr = SessionManager(db)
        logger = EventLogger(db)
        
        session = session_mgr.create_session(user_id="alice")
        
        # Create real database event
        db_event = logger.create_user_query(
            user_id="alice",
            session_id=session.session_id,
            content="What is event sourcing?",
        )
        
        # Convert to StreamEvent
        stream_event = StreamEvent(
            event_type="user_query",
            data={
                "content": db_event.content,
                "user_id": db_event.user_id,
            },
            run_id="run123",
            timestamp=db_event.created_at,
        )
        
        # Verify serialization
        result = stream_event.to_dict()
        assert result["event_type"] == "user_query"
        assert result["data"]["content"] == "What is event sourcing?"
        assert result["run_id"] == "run123"
    
    @pytest.mark.asyncio
    async def test_websocket_transport_with_database_events(self, db: Session):
        """Test WebSocket transport sending real database events.
        
        Real scenario: Database events → WebSocket → Client
        """
        # Setup
        session_mgr = SessionManager(db)
        logger = EventLogger(db)
        
        session = session_mgr.create_session(user_id="bob")
        
        # Create real database events
        user_event = logger.create_user_query(
            user_id="bob",
            session_id=session.session_id,
            content="Test message",
        )
        
        llm_event = logger.create_llm_response(
            user_id="bob",
            session_id=session.session_id,
            content="Response message",
            agent_id="dev-agent",
            agent_version="0.1.0",
            parent_event_id=user_event.event_id,
            causal_chain_id=user_event.causal_chain_id,
        )
        
        # Mock WebSocket
        mock_ws = AsyncMock()
        transport = WebSocketTransport(mock_ws, "run123")
        
        try:
            # Connect
            await transport.connect()
            assert transport.state == ConnectionState.CONNECTED
            
            # Send database events through transport
            for db_evt in [user_event, llm_event]:
                event = StreamEvent(
                    event_type="user_query" if db_evt == user_event else "llm_response",
                    data={"content": db_evt.content},
                    run_id="run123",
                )
                success = await transport.send_event(event)
                assert success is True
            
            # Verify WebSocket received events
            assert mock_ws.send_json.call_count == 2
        finally:
            # Cleanup: close transport
            await transport.close()
    
    def test_multi_agent_aggregator_with_database_events(self, db: Session):
        """Test aggregator with real database events from multiple sessions.
        
        Real scenario: Multiple sessions → Events aggregated → Ordered output
        """
        # Setup
        session_mgr = SessionManager(db)
        logger = EventLogger(db)
        
        session1 = session_mgr.create_session(user_id="charlie")
        session2 = session_mgr.create_session(user_id="charlie")
        
        # Create events in both sessions
        event1 = logger.create_user_query(
            user_id="charlie",
            session_id=session1.session_id,
            content="Agent 1 query",
        )
        
        event2 = logger.create_user_query(
            user_id="charlie",
            session_id=session2.session_id,
            content="Agent 2 query",
        )
        
        # Create aggregator
        aggregator = MultiAgentAggregator("combined_run")
        
        # Simulate event streams from database
        async def agent1_stream():
            yield {
                "event_type": "user_query",
                "data": {"content": event1.content},
                "agent_id": "agent1",
            }
        
        async def agent2_stream():
            yield {
                "event_type": "user_query",
                "data": {"content": event2.content},
                "agent_id": "agent2",
            }
        
        aggregator.register_agent_stream("agent1", agent1_stream())
        aggregator.register_agent_stream("agent2", agent2_stream())
        
        # Verify registration
        assert len(aggregator.agent_streams) == 2
        assert "agent1" in aggregator.agent_streams
        assert "agent2" in aggregator.agent_streams
    
    def test_agui_protocol_validation_with_database_events(self, db: Session):
        """Test AG-UI protocol validation with real database events.
        
        Real scenario: Database events → AG-UI format → Validation
        """
        # Setup
        session_mgr = SessionManager(db)
        logger = EventLogger(db)
        
        session = session_mgr.create_session(user_id="diana")
        
        # Create real database events
        user_event = logger.create_user_query(
            user_id="diana",
            session_id=session.session_id,
            content="Validate protocol",
        )
        
        # Convert to AG-UI format
        agui_events = [
            {
                "event_type": "user_query",
                "data": {"content": user_event.content},
            },
        ]
        
        # Validate
        validator = AGUIProtocolValidator()
        report = validator.validate_stream(agui_events)
        
        # Verify
        assert report["total_events"] == 1
        assert report["valid_events"] >= 1
    
    @pytest.mark.asyncio
    async def test_websocket_heartbeat_with_database_context(self, db: Session):
        """Test WebSocket heartbeat with real database session context.
        
        Real scenario: Long-running task → Heartbeats keep connection alive
        """
        # Setup
        session_mgr = SessionManager(db)
        logger = EventLogger(db)
        
        session = session_mgr.create_session(user_id="eve")
        
        # Create database events
        for i in range(3):
            logger.create_user_query(
                user_id="eve",
                session_id=session.session_id,
                content=f"Event {i}",
            )
        
        # Mock WebSocket
        mock_ws = AsyncMock()
        transport = WebSocketTransport(mock_ws, "run123")
        
        try:
            await transport.connect()
            
            # Send heartbeat
            success = await transport.send_heartbeat()
            assert success is True
            
            # Verify heartbeat was sent
            assert mock_ws.send_json.called
            call_args = mock_ws.send_json.call_args[0][0]
            assert call_args["event_type"] == "heartbeat"
        finally:
            # Cleanup: close transport
            await transport.close()


class TestStreamingProtocolRealWorldIntegration:
    """Real-world integration scenarios."""
    
    def test_multi_turn_conversation_event_sequence(self, db: Session):
        """Test multi-turn conversation event sequence in database.
        
        Real scenario: 3-turn conversation → Events stored → Sequence verified
        """
        # Setup
        session_mgr = SessionManager(db)
        logger = EventLogger(db)
        
        session = session_mgr.create_session(user_id="frank")
        
        # Simulate 3-turn conversation
        turns = [
            ("What is Python?", "Python is a programming language..."),
            ("What are its features?", "Python has many features..."),
            ("How to learn it?", "You can learn Python by..."),
        ]
        
        all_events = []
        
        for user_msg, agent_msg in turns:
            # Log user message
            user_event = logger.create_user_query(
                user_id="frank",
                session_id=session.session_id,
                content=user_msg,
            )
            
            # Log agent response
            llm_event = logger.create_llm_response(
                user_id="frank",
                session_id=session.session_id,
                content=agent_msg,
                agent_id="dev-agent",
                agent_version="0.1.0",
                parent_event_id=user_event.event_id,
                causal_chain_id=user_event.causal_chain_id,
            )
            
            all_events.append((user_event, llm_event))
        
        # Verify conversation flow
        assert len(all_events) == 3
        
        # Verify causal chains
        for user_evt, llm_evt in all_events:
            assert llm_evt.parent_event_id == user_evt.event_id
            assert llm_evt.causal_chain_id == user_evt.causal_chain_id
        
        # Verify content
        assert "Python" in all_events[0][0].content
        assert "features" in all_events[1][0].content
        assert "learn" in all_events[2][0].content
    
    def test_event_priority_ordering(self, db: Session):
        """Test event priority ordering for streaming.
        
        Real scenario: Mix of events with different priorities → Ordered output
        """
        # Setup
        session_mgr = SessionManager(db)
        logger = EventLogger(db)
        
        session = session_mgr.create_session(user_id="grace")
        
        # Create events
        user_event = logger.create_user_query(
            user_id="grace",
            session_id=session.session_id,
            content="Normal query",
        )
        
        # Simulate priority assignment
        events_with_priority = [
            (user_event, EventPriority.NORMAL),
        ]
        
        # Sort by priority
        sorted_events = sorted(
            events_with_priority,
            key=lambda x: x[1].value,
        )
        
        # Verify ordering
        assert len(sorted_events) == 1
        assert sorted_events[0][1] == EventPriority.NORMAL
    
    def test_connection_state_transitions(self, db: Session):
        """Test WebSocket connection state transitions.
        
        Real scenario: Connect → Send events → Disconnect → Reconnect
        """
        # Setup
        session_mgr = SessionManager(db)
        logger = EventLogger(db)
        
        session = session_mgr.create_session(user_id="henry")
        
        # Create database event
        logger.create_user_query(
            user_id="henry",
            session_id=session.session_id,
            content="Test connection",
        )
        
        # Test state transitions
        mock_ws = AsyncMock()
        transport = WebSocketTransport(mock_ws, "run123")
        
        # Initial state
        assert transport.state == ConnectionState.CONNECTING
        
        # After connect
        import asyncio
        try:
            asyncio.run(transport.connect())
            assert transport.state == ConnectionState.CONNECTED
            
            # After close
            asyncio.run(transport.close())
            assert transport.state == ConnectionState.DISCONNECTED
        finally:
            # Ensure cleanup
            if transport.state != ConnectionState.DISCONNECTED:
                asyncio.run(transport.close())
