"""Batch event writer for high throughput."""

import threading
import time
from queue import Queue
from typing import List, Dict, Any

from sdk import Database
from core.logging_config import get_logger

logger = get_logger(__name__)


class BatchEventWriter:
    """Batch writer for conversation events."""

    def __init__(self, db: Database, batch_size: int = 100, flush_interval: float = 1.0):
        """Initialize batch writer.

        Args:
            db: Database instance
            batch_size: Max events per batch
            flush_interval: Max seconds between flushes
        """
        self.db = db
        self.batch_size = batch_size
        self.flush_interval = flush_interval

        self._queue = Queue()
        self._buffer: List[Dict[str, Any]] = []
        self._lock = threading.Lock()
        self._running = False
        self._thread = None
        self._last_flush = time.time()

    def start(self):
        """Start background writer thread."""
        if self._running:
            return

        self._running = True
        self._thread = threading.Thread(target=self._writer_loop, daemon=True)
        self._thread.start()
        logger.info("Batch event writer started")

    def stop(self):
        """Stop writer and flush remaining events."""
        self._running = False
        if self._thread:
            self._thread.join(timeout=5)
        self._flush()
        logger.info("Batch event writer stopped")

    def write_event(self, event: Dict[str, Any]):
        """Queue event for batch writing."""
        self._queue.put(event)

    def _writer_loop(self):
        """Background writer loop."""
        while self._running:
            try:
                # Get events from queue
                while not self._queue.empty() and len(self._buffer) < self.batch_size:
                    event = self._queue.get(timeout=0.1)
                    self._buffer.append(event)

                # Flush if batch full or interval elapsed
                should_flush = len(self._buffer) >= self.batch_size or (
                    self._buffer and time.time() - self._last_flush >= self.flush_interval
                )

                if should_flush:
                    self._flush()

                time.sleep(0.1)

            except Exception as e:
                logger.error(f"Batch writer error: {e}")

    def _flush(self):
        """Flush buffered events to database."""
        if not self._buffer:
            return

        with self._lock:
            events = self._buffer[:]
            self._buffer.clear()

        try:
            # Batch insert
            if events:
                self._batch_insert(events)
                self._last_flush = time.time()
                logger.debug(f"Flushed {len(events)} events")

        except Exception as e:
            logger.error(f"Batch flush failed: {e}")
            # Re-queue events
            for event in events:
                self._queue.put(event)

    def _batch_insert(self, events: List[Dict[str, Any]]):
        """Insert events in batch."""
        if not events:
            return

        # Build batch INSERT
        placeholders = ", ".join("(%s, %s, %s, %s, %s, %s, %s, %s, %s, %s, %s)" for _ in events)

        query = f"""
            INSERT INTO conversation_events (
                event_id, user_id, session_id, agent_id, agent_version,
                event_type, content, parent_event_id, causal_chain_id,
                created_at, metadata
            ) VALUES {placeholders}
        """

        # Flatten parameters
        params = []
        for event in events:
            params.extend(
                [
                    event["event_id"],
                    event["user_id"],
                    event["session_id"],
                    event["agent_id"],
                    event["agent_version"],
                    event["event_type"],
                    event["content"],
                    event.get("parent_event_id"),
                    event.get("causal_chain_id"),
                    event.get("created_at"),
                    event.get("metadata"),
                ]
            )

        self.db.execute(query, tuple(params))


# Global batch writer
_writer = None


def get_batch_writer(db: Database = None):
    """Get global batch writer instance."""
    global _writer
    if _writer is None and db:
        _writer = BatchEventWriter(db)
        _writer.start()
    return _writer
