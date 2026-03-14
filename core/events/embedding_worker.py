"""Async embedding worker — decoupled from the write path.

Polls agent_events LEFT JOIN ctx_event_embeddings for events that
need embeddings, generates them, and writes to ctx_event_embeddings.

Only embeds: user_query, llm_response, plan_created, knowledge_extracted.
"""

import asyncio
import logging
import time
from collections.abc import Callable

from sqlalchemy import text
from sqlalchemy.orm import Session

logger = logging.getLogger(__name__)

# Event types worth embedding (content is semantically meaningful)
EMBED_EVENT_TYPES = (
    "user_query",
    "llm_response",
    "tool_result",
    "plan_created",
    "knowledge_extracted",
)

# Poll interval and batch size
_POLL_INTERVAL_S = 1.0
_BATCH_SIZE = 50


class EmbeddingWorker:
    """Background worker that generates embeddings for new events.

    Runs as an asyncio task. Polls for events missing embeddings,
    generates them via EmbeddingService, and writes to ctx_event_embeddings.
    """

    def __init__(
        self,
        db_factory: Callable[[], Session],
        embedding_provider: str = "openai",
        poll_interval: float = _POLL_INTERVAL_S,
        batch_size: int = _BATCH_SIZE,
    ) -> None:
        self._db_factory = db_factory
        self._provider = embedding_provider
        self._poll_interval = poll_interval
        self._batch_size = batch_size
        self._task: asyncio.Task | None = None
        self._closed = False

    def start(self) -> None:
        """Start the background polling loop."""
        if self._task is None or self._task.done():
            try:
                loop = asyncio.get_running_loop()
                self._task = loop.create_task(self._poll_loop())
            except RuntimeError:
                pass

    def stop(self) -> "asyncio.Task | None":
        """Signal stop. Returns task for awaiting."""
        self._closed = True
        task = self._task
        self._task = None
        return task

    async def _poll_loop(self) -> None:
        """Poll for unembedded events and process them."""
        db = self._db_factory()
        try:
            from core.context.embeddings import EmbeddingService

            svc = EmbeddingService(self._db_factory)

            while not self._closed:
                try:
                    count = self._process_batch(db, svc)
                    if count == 0:
                        await asyncio.sleep(self._poll_interval)
                    # If we got a full batch, loop immediately for more
                except Exception:
                    db.rollback()
                    logger.exception("Embedding batch failed")
                    await asyncio.sleep(self._poll_interval * 2)
        except asyncio.CancelledError:
            pass
        finally:
            db.close()

    def _process_batch(self, db: Session, svc) -> int:
        """Find unembedded events, generate embeddings, write to ctx_event_embeddings.

        Returns number of events processed.
        """
        type_placeholders = ", ".join(f":t{i}" for i in range(len(EMBED_EVENT_TYPES)))
        params = {f"t{i}": t for i, t in enumerate(EMBED_EVENT_TYPES)}
        params["limit"] = self._batch_size

        rows = db.execute(
            text(f"""
                SELECT ce.event_id, ce.content
                FROM agent_events ce
                LEFT JOIN ctx_event_embeddings ee ON ce.event_id = ee.event_id
                WHERE ee.event_id IS NULL
                  AND ce.event_type IN ({type_placeholders})
                  AND ce.content IS NOT NULL
                  AND ce.content != ''
                ORDER BY ce.created_at ASC
                LIMIT :limit
            """),
            params,
        ).fetchall()

        if not rows:
            return 0

        for row in rows:
            try:
                vec = svc.embed_text(row.content)
                vec_str = "[" + ",".join(str(v) for v in vec) + "]"
                db.execute(
                    text("""
                        INSERT INTO ctx_event_embeddings
                        (event_id, embedding, model_name, model_version, metadata, created_at, updated_at)
                        VALUES (:event_id, :embedding, :model, '1.0', '{}', NOW(), NOW())
                    """),
                    {"event_id": row.event_id, "embedding": vec_str, "model": svc.model},
                )
            except Exception:
                logger.warning(
                    "Failed to embed event %s — marking as skipped to prevent retry loop",
                    row.event_id,
                    exc_info=True,
                )
                # Insert a sentinel row so this event is not retried indefinitely
                try:
                    db.execute(
                        text("""
                            INSERT INTO ctx_event_embeddings
                            (event_id, embedding, model_name, model_version, metadata, created_at, updated_at)
                            VALUES (:event_id, NULL, 'error', '0', '{"error": "embed_failed"}', NOW(), NOW())
                        """),
                        {"event_id": row.event_id},
                    )
                except Exception:
                    pass  # If sentinel insert also fails, it will retry next cycle — acceptable

        db.commit()
        logger.info("Embedded %d events", len(rows))
        return len(rows)

    def process_batch_sync(self, db: Session | None = None) -> int:
        """Synchronous single-batch processing (for tests and migration).

        Args:
            db: Optional session. If None, creates one from factory.

        Returns:
            Number of events processed.
        """
        own_db = db is None
        if own_db:
            db = self._db_factory()
        try:
            from core.context.embeddings import EmbeddingService

            svc = EmbeddingService(self._db_factory)
            return self._process_batch(db, svc)
        finally:
            if own_db:
                db.close()
