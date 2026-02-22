"""WebSocket transport layer for streaming events.

Minimal implementation: connection management, event serialization, error handling.
"""

from __future__ import annotations

import asyncio
import json
import logging
from dataclasses import dataclass
from datetime import datetime
from enum import Enum
from typing import AsyncIterator, Callable, Optional

from fastapi import WebSocket, WebSocketDisconnect

logger = logging.getLogger(__name__)


class ConnectionState(str, Enum):
    """WebSocket connection state."""
    CONNECTING = "connecting"
    CONNECTED = "connected"
    DISCONNECTING = "disconnecting"
    DISCONNECTED = "disconnected"


@dataclass
class StreamEvent:
    """Structured stream event."""
    event_type: str
    data: dict
    timestamp: datetime | None = None
    run_id: str | None = None
    
    def to_dict(self) -> dict:
        """Serialize to dict."""
        return {
            "event_type": self.event_type,
            "data": self.data,
            "timestamp": self.timestamp.isoformat() if self.timestamp else None,
            "run_id": self.run_id,
        }


class WebSocketTransport:
    """Manage WebSocket connection lifecycle and event transmission."""
    
    def __init__(self, websocket: WebSocket, run_id: str):
        """Initialize transport.
        
        Args:
            websocket: FastAPI WebSocket connection
            run_id: Associated run ID
        """
        self.websocket = websocket
        self.run_id = run_id
        self.state = ConnectionState.CONNECTING
        self.last_heartbeat = datetime.now()
        self.heartbeat_interval = 30  # seconds
        self.event_count = 0
    
    async def connect(self) -> bool:
        """Establish connection.
        
        Returns:
            True if successful, False otherwise
        """
        try:
            await self.websocket.accept()
            self.state = ConnectionState.CONNECTED
            logger.info(f"WebSocket connected: run_id={self.run_id}")
            return True
        except Exception as e:
            logger.error(f"WebSocket accept failed: {e}")
            self.state = ConnectionState.DISCONNECTED
            return False
    
    async def send_event(self, event: StreamEvent) -> bool:
        """Send event to client.
        
        Args:
            event: Event to send
            
        Returns:
            True if successful, False if connection closed
        """
        if self.state != ConnectionState.CONNECTED:
            return False
        
        try:
            await self.websocket.send_json(event.to_dict())
            self.event_count += 1
            return True
        except Exception as e:
            logger.error(f"Send failed: {e}")
            self.state = ConnectionState.DISCONNECTED
            return False
    
    async def send_heartbeat(self) -> bool:
        """Send heartbeat to keep connection alive.
        
        Returns:
            True if successful
        """
        event = StreamEvent(
            event_type="heartbeat",
            data={"timestamp": datetime.now().isoformat()},
            run_id=self.run_id,
        )
        return await self.send_event(event)
    
    async def receive_message(self) -> Optional[dict]:
        """Receive message from client (non-blocking).
        
        Returns:
            Message dict or None if no message
        """
        if self.state != ConnectionState.CONNECTED:
            return None
        
        try:
            # Try to receive with timeout
            data = await asyncio.wait_for(
                self.websocket.receive_json(),
                timeout=0.1
            )
            return data
        except asyncio.TimeoutError:
            return None
        except WebSocketDisconnect:
            self.state = ConnectionState.DISCONNECTED
            return None
        except Exception as e:
            logger.error(f"Receive failed: {e}")
            self.state = ConnectionState.DISCONNECTED
            return None
    
    async def close(self, code: int = 1000, reason: str = "Normal closure"):
        """Close connection gracefully.
        
        Args:
            code: WebSocket close code
            reason: Close reason
        """
        if self.state == ConnectionState.DISCONNECTED:
            return
        
        self.state = ConnectionState.DISCONNECTING
        try:
            await self.websocket.close(code=code, reason=reason)
            logger.info(f"WebSocket closed: run_id={self.run_id}, events_sent={self.event_count}")
        except Exception as e:
            logger.error(f"Close failed: {e}")
        finally:
            self.state = ConnectionState.DISCONNECTED
    
    async def stream_with_heartbeat(
        self,
        event_source: AsyncIterator[StreamEvent],
        heartbeat_interval: int = 30,
    ) -> None:
        """Stream events with periodic heartbeats.
        
        Args:
            event_source: Async iterator of events
            heartbeat_interval: Seconds between heartbeats
        """
        heartbeat_task = None
        try:
            async def send_heartbeats():
                """Send heartbeats periodically."""
                while self.state == ConnectionState.CONNECTED:
                    await asyncio.sleep(heartbeat_interval)
                    if self.state == ConnectionState.CONNECTED:
                        await self.send_heartbeat()
            
            # Start heartbeat task
            heartbeat_task = asyncio.create_task(send_heartbeats())
            
            # Stream events
            async for event in event_source:
                if not await self.send_event(event):
                    break
        
        except Exception as e:
            logger.error(f"Stream error: {e}")
            await self.send_event(StreamEvent(
                event_type="error",
                data={"error": str(e)},
                run_id=self.run_id,
            ))
        finally:
            if heartbeat_task:
                heartbeat_task.cancel()
                try:
                    await heartbeat_task
                except asyncio.CancelledError:
                    pass
            await self.close()
