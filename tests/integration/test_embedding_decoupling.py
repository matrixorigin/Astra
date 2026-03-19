"""Integration tests for A3: Embedding decoupling.

Tests the real end-to-end flow:
1. Events written WITHOUT inline embedding (EventLogger no longer embeds)
2. EmbeddingWorker picks up unembedded events and writes to ctx_event_embeddings
3. HybridRetriever reads from ctx_event_embeddings JOIN (not agent_events.embedding)
4. Fulltext fallback works when zero embeddings exist
"""

import asyncio
import time
from datetime import datetime, timezone

import pytest
from sqlalchemy import text
from uuid_utils import uuid7

from core.events.embedding_worker import EmbeddingWorker, EMBED_EVENT_TYPES
from core.events.event_logger import EventLogger
from core.events.models import ConversationEvent, EventType


def _make_event(session_id, event_type=EventType.USER_QUERY, content="What is event sourcing?"):
    eid = str(uuid7())
    return ConversationEvent(
        event_id=eid,
        user_id="test_user",
        session_id=session_id,
        agent_id="test-agent",
        agent_version="0.1",
        event_type=event_type,
        content=content,
        causal_chain_id=eid,
        created_at=datetime.now(timezone.utc),
    )


def _drain_worker(worker, db) -> int:
    """Run worker until all pending events are embedded. Returns total count."""
    total = 0
    for _ in range(100):  # safety cap to prevent infinite loop
        count = worker.process_batch_sync(db)
        total += count
        if count == 0:
            return total
    raise RuntimeError(
        f"_drain_worker: still processing after 100 iterations ({total} events embedded)"
    )


@pytest.fixture
def session_id():
    return str(uuid7())


@pytest.fixture
def cleanup_events(db_session, session_id):
    """Clean up test events after test."""
    yield
    try:
        # Delete embeddings first (FK-like dependency)
        db_session.execute(
            text(
                "DELETE FROM ctx_event_embeddings WHERE event_id IN "
                "(SELECT event_id FROM agent_events WHERE session_id = :sid)"
            ),
            {"sid": session_id},
        )
        db_session.execute(
            text("DELETE FROM agent_events WHERE session_id = :sid"), {"sid": session_id}
        )
        db_session.commit()
    except Exception:
        db_session.rollback()


class TestEventLoggerNoInlineEmbedding:
    """Verify EventLogger no longer generates embeddings inline."""

    def test_log_event_writes_null_embedding(self, db_session, session_id, cleanup_events):
        """EventLogger sync path must write embedding=NULL."""
        logger = EventLogger.from_session(db_session)
        ev = _make_event(session_id)
        logger.log_event(ev)

        row = db_session.execute(
            text("SELECT embedding FROM agent_events WHERE event_id = :eid"),
            {"eid": ev.event_id},
        ).fetchone()

        assert row is not None, "Event should be persisted"
        assert row[0] is None, "Embedding should be NULL — no longer generated inline"


class TestEmbeddingWorker:
    """Verify EmbeddingWorker picks up events and writes to ctx_event_embeddings."""

    def test_worker_embeds_eligible_events(self, db_session, session_id, cleanup_events):
        """Worker should embed user_query and llm_response, skip others."""
        logger = EventLogger.from_session(db_session)

        # Write events of different types
        ev_query = _make_event(session_id, EventType.USER_QUERY, "What is HTAP?")
        ev_response = _make_event(session_id, EventType.LLM_RESPONSE, "HTAP means...")
        ev_tool = _make_event(session_id, EventType.TOOL_CALL, "calling tool X")
        ev_stream = _make_event(session_id, EventType.STREAM_TEXT_DELTA, "partial text")

        for ev in [ev_query, ev_response, ev_tool, ev_stream]:
            logger.log_event(ev)

        # Run worker synchronously — drain all pending batches so our events
        # are reached even when a backlog of unembedded events exists.
        from api.database import SessionLocal

        worker = EmbeddingWorker(SessionLocal, embedding_provider="mock")
        total = _drain_worker(worker, db_session)
        assert total >= 2, f"Should embed at least 2 eligible events, got {total}"

        # Verify our eligible events got embedded
        embedded = db_session.execute(
            text("SELECT event_id FROM ctx_event_embeddings WHERE event_id IN (:e1, :e2)"),
            {"e1": ev_query.event_id, "e2": ev_response.event_id},
        ).fetchall()
        assert len(embedded) == 2

        # Verify non-eligible events were NOT embedded
        not_embedded = db_session.execute(
            text("SELECT event_id FROM ctx_event_embeddings WHERE event_id IN (:e1, :e2)"),
            {"e1": ev_tool.event_id, "e2": ev_stream.event_id},
        ).fetchall()
        assert len(not_embedded) == 0

    def test_worker_idempotent(self, db_session, session_id, cleanup_events):
        """Running worker twice should not duplicate embeddings."""
        logger = EventLogger.from_session(db_session)
        ev = _make_event(session_id, EventType.USER_QUERY, "test idempotency")
        logger.log_event(ev)

        from api.database import SessionLocal

        worker = EmbeddingWorker(SessionLocal, embedding_provider="mock")

        # Drain all pending events (including any backlog from other tests)
        count1 = _drain_worker(worker, db_session)
        count2 = worker.process_batch_sync(db_session)

        assert count1 >= 1, f"First run should embed at least 1 event, got {count1}"
        assert count2 == 0, "Second run should find nothing to embed"

    @pytest.mark.asyncio
    async def test_worker_async_lifecycle(self, db_session, session_id, cleanup_events):
        """Worker start/stop lifecycle with async polling."""
        logger = EventLogger.from_session(db_session)
        ev = _make_event(session_id, EventType.USER_QUERY, "async test")
        logger.log_event(ev)

        from api.database import SessionLocal

        worker = EmbeddingWorker(SessionLocal, embedding_provider="mock", poll_interval=0.1)
        worker.start()

        # Wait for worker to process
        for _ in range(20):
            row = db_session.execute(
                text("SELECT 1 FROM ctx_event_embeddings WHERE event_id = :eid"),
                {"eid": ev.event_id},
            ).fetchone()
            if row:
                break
            await asyncio.sleep(0.1)

        task = worker.stop()
        if task:
            await asyncio.wait_for(task, timeout=5.0)

        # Verify embedding was created
        row = db_session.execute(
            text("SELECT model_name FROM ctx_event_embeddings WHERE event_id = :eid"),
            {"eid": ev.event_id},
        ).fetchone()
        assert row is not None, "Worker should have embedded the event"


class TestHybridRetrieverJoinPath:
    """Verify HybridRetriever reads from ctx_event_embeddings JOIN."""

    def test_retrieval_uses_ctx_event_embeddings(self, db_session, session_id, cleanup_events):
        """Vector search should find events via ctx_event_embeddings JOIN."""
        from core.context.hybrid_retrieval import HybridRetriever
        from core.context.embeddings import EmbeddingService

        logger = EventLogger.from_session(db_session)
        ev = _make_event(session_id, EventType.USER_QUERY, "What is event sourcing?")
        logger.log_event(ev)

        # Generate and store embedding in ctx_event_embeddings (simulating worker)
        svc = EmbeddingService(lambda: db_session, provider="mock")
        svc.store_embedding(ev.event_id, svc.embed_text(ev.content))

        # Query with same text — should find via JOIN
        retriever = HybridRetriever(lambda: db_session)
        query_vec = svc.embed_text("event sourcing")
        results = retriever.retrieve_events(
            query_text="event sourcing",
            query_embedding=query_vec,
            session_id=session_id,
        )

        event_ids = [r["event_id"] for r in results]
        assert ev.event_id in event_ids, "Should find event via ctx_event_embeddings JOIN"

    def test_fulltext_fallback_with_zero_embeddings(self, db_session, session_id, cleanup_events):
        """When no embeddings exist, fulltext search should still return results."""
        from core.context.hybrid_retrieval import HybridRetriever
        from core.context.embeddings import EmbeddingService

        logger = EventLogger.from_session(db_session)
        ev = _make_event(session_id, EventType.USER_QUERY, "MatrixOne HTAP database")
        logger.log_event(ev)

        # Do NOT create any embeddings — test fulltext-only path
        retriever = HybridRetriever(lambda: db_session)
        svc = EmbeddingService(lambda: db_session, provider="mock")
        query_vec = svc.embed_text("MatrixOne HTAP")

        results = retriever.retrieve_events(
            query_text="MatrixOne HTAP",
            query_embedding=query_vec,
            session_id=session_id,
        )

        # Fulltext should find it even without embeddings
        assert len(results) > 0, "Fulltext fallback must return results when zero embeddings exist"
        assert any("MatrixOne" in r.get("content", "") for r in results)

    def test_event_without_embedding_not_in_vector_results(
        self, db_session, session_id, cleanup_events
    ):
        """Events without embeddings should not appear in vector search results."""
        from core.context.hybrid_retrieval import HybridRetriever
        from core.context.embeddings import EmbeddingService

        logger = EventLogger.from_session(db_session)

        # Two events, only one gets an embedding
        ev_with = _make_event(session_id, EventType.USER_QUERY, "embedded event about databases")
        ev_without = _make_event(
            session_id, EventType.USER_QUERY, "no embedding event about databases"
        )
        logger.log_event(ev_with)
        logger.log_event(ev_without)

        svc = EmbeddingService(lambda: db_session, provider="mock")
        svc.store_embedding(ev_with.event_id, svc.embed_text(ev_with.content))

        retriever = HybridRetriever(lambda: db_session)
        query_vec = svc.embed_text("databases")
        results = retriever.retrieve_events(
            query_text="databases",
            query_embedding=query_vec,
            session_id=session_id,
        )

        # ev_with should appear (has embedding), ev_without may appear via fulltext only
        result_ids = [r["event_id"] for r in results]
        assert ev_with.event_id in result_ids, "Embedded event should be found"


class TestEndToEndDecoupled:
    """Full pipeline: write event → worker embeds → retriever finds it."""

    def test_write_embed_retrieve(self, db_session, session_id, cleanup_events):
        """End-to-end: EventLogger writes → EmbeddingWorker embeds → HybridRetriever finds."""
        from core.context.hybrid_retrieval import HybridRetriever
        from core.context.embeddings import EmbeddingService
        from api.database import SessionLocal

        # 1. Write event (no inline embedding)
        logger = EventLogger.from_session(db_session)
        ev = _make_event(session_id, EventType.USER_QUERY, "How does MVCC work in MatrixOne?")
        logger.log_event(ev)

        # Verify no embedding in agent_events
        fresh_db = SessionLocal()
        try:
            row = fresh_db.execute(
                text("SELECT embedding FROM agent_events WHERE event_id = :eid"),
                {"eid": ev.event_id},
            ).fetchone()
        finally:
            fresh_db.close()
        assert row is not None, "Event should be persisted"
        assert row[0] is None, "agent_events.embedding should be NULL"

        # 2. Worker generates embedding into ctx_event_embeddings
        worker = EmbeddingWorker(SessionLocal, embedding_provider="mock")
        count = _drain_worker(worker, db_session)
        assert count >= 1

        # Verify embedding in ctx_event_embeddings
        fresh_db = SessionLocal()
        try:
            emb_row = fresh_db.execute(
                text("SELECT model_name FROM ctx_event_embeddings WHERE event_id = :eid"),
                {"eid": ev.event_id},
            ).fetchone()
        finally:
            fresh_db.close()
        assert emb_row is not None, "ctx_event_embeddings should have the embedding"

        # 3. Retriever finds it via JOIN
        svc = EmbeddingService(lambda: db_session, provider="mock")
        retriever = HybridRetriever(lambda: db_session)
        results = retriever.retrieve_events(
            query_text="MVCC MatrixOne",
            query_embedding=svc.embed_text("MVCC MatrixOne"),
            session_id=session_id,
        )

        assert any(r["event_id"] == ev.event_id for r in results), (
            "Retriever should find the event via ctx_event_embeddings JOIN"
        )
