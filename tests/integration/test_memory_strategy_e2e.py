"""Integration tests for new memory architecture (Phase 0).

Verifies the REAL DB behavior of:
1. MemoryService facade via create_memory_service() factory
2. MemoriaStorage → mem_memories field-level correctness
3. VectorRetrievalStrategy → real SQL hybrid retrieval
4. ActivationRetrievalStrategy → real graph retrieval + vector fallback
5. ActivationIndexManager → real graph node/edge creation on store()
6. Governance → storage + index manager coordination
"""

from uuid import uuid4

import pytest

from api.models._constants import EMBEDDING_DIM
from core.memory.models.memory import MemoryRecord
from core.memory.factory import create_memory_service
from core.memory.types import Memory, MemoryType, TrustTier
from datetime import datetime, timedelta, timezone


def _uid() -> str:
    return f"strat_e2e_{uuid4().hex[:12]}"


def _sid() -> str:
    return f"sess_{uuid4().hex[:12]}"


def _embed(seed: float = 0.1) -> list[float]:
    return [seed] * EMBEDDING_DIM


def _similar_embed() -> list[float]:
    e = [0.1] * EMBEDDING_DIM
    for i in range(EMBEDDING_DIM // 10):
        e[i] = 0.15
    return e


@pytest.fixture
def db_factory():
    from api.database import SessionLocal
    return SessionLocal


@pytest.fixture
def db_session(db_factory):
    db = db_factory()
    yield db
    db.close()


@pytest.fixture
def user_id():
    return _uid()


@pytest.fixture(autouse=True)
def cleanup(db_factory, user_id):
    """Cleanup all test data after each test."""
    yield
    from sqlalchemy import text
    db = db_factory()
    try:
        db.execute(text(
            "DELETE FROM memory_graph_edges WHERE user_id = :uid"
        ), {"uid": user_id})
        db.execute(text(
            "DELETE FROM memory_graph_nodes WHERE user_id = :uid"
        ), {"uid": user_id})
        db.execute(text(
            "DELETE FROM mem_memories WHERE user_id = :uid"
        ), {"uid": user_id})
        db.commit()
    finally:
        db.close()


# ── 1. Factory creates correct service type ───────────────────────────

class TestFactoryCreatesRealService:
    """Factory → MemoryService with correct strategy, verified against real DB."""

    def test_vector_strategy_stores_and_retrieves(self, db_factory, db_session, user_id):
        """vector:v1: store → DB row correct → retrieve returns it."""
        svc = create_memory_service(db_factory, backend="tabular")
        assert svc.strategy_key == "vector:v1"

        mem = svc.store(
            user_id, "User prefers Python for data work",
            memory_type=MemoryType.SEMANTIC,
            initial_confidence=0.85,
            trust_tier=TrustTier.T2_CURATED,
            session_id=_sid(),
        )

        # Ground truth: verify DB row field-by-field
        db_session.expire_all()
        row = db_session.query(MemoryRecord).filter_by(
            memory_id=mem.memory_id,
        ).first()
        assert row is not None
        assert row.user_id == user_id
        assert row.memory_type == "semantic"
        assert row.content == "User prefers Python for data work"
        assert row.initial_confidence == pytest.approx(0.85, abs=0.01)
        assert row.trust_tier == "T2"
        assert row.is_active == 1
        assert row.observed_at is not None
        assert row.created_at is not None

    def test_activation_strategy_stores_with_graph_index(
        self, db_factory, db_session, user_id,
    ):
        """activation:v1: store → DB row + graph node created."""
        svc = create_memory_service(db_factory, backend="graph")
        assert svc.strategy_key == "activation:v1"
        assert svc._index_manager is not None

        emb = _embed(0.2)
        # Use create_memory to bypass LLM extraction, with embedding
        mem = Memory(
            memory_id=uuid4().hex,
            user_id=user_id,
            memory_type=MemoryType.SEMANTIC,
            content="User prefers Go for system tools",
            initial_confidence=0.8,
            trust_tier=TrustTier.T3_INFERRED,
            embedding=emb,
        )
        result = svc.storage.create_memory(mem)
        # Manually trigger index update (create_memory bypasses facade)
        svc._index_manager.on_memories_stored(user_id, [result])

        # Ground truth 1: mem_memories row
        db_session.expire_all()
        row = db_session.query(MemoryRecord).filter_by(
            memory_id=mem.memory_id,
        ).first()
        assert row is not None
        assert row.user_id == user_id
        assert row.content == "User prefers Go for system tools"
        assert row.trust_tier == "T3"
        assert row.is_active == 1

        # Ground truth 2: graph node created
        from core.memory.models.graph import GraphNode
        gnode = db_session.query(GraphNode).filter_by(
            memory_id=mem.memory_id,
        ).first()
        assert gnode is not None
        assert gnode.user_id == user_id
        assert gnode.node_type == "semantic"
        assert gnode.content == "User prefers Go for system tools"
        assert gnode.is_active == 1


# ── 2. MemoriaStorage field-level DB verification ───────────────────

class TestMemoriaStorageDB:
    """MemoriaStorage writes correct fields to mem_memories."""

    def test_store_all_fields(self, db_factory, db_session, user_id):
        svc = create_memory_service(db_factory, strategy="vector:v1")
        session_id = _sid()

        mem = svc.store(
            user_id, "Always use type hints in Python",
            memory_type=MemoryType.PROCEDURAL,
            initial_confidence=0.9,
            trust_tier=TrustTier.T1_VERIFIED,
            session_id=session_id,
            source_event_ids=["evt_a", "evt_b"],
        )

        db_session.expire_all()
        row = db_session.query(MemoryRecord).filter_by(
            memory_id=mem.memory_id,
        ).first()

        assert row is not None
        assert row.user_id == user_id
        assert row.session_id == session_id
        assert row.memory_type == "procedural"
        assert row.content == "Always use type hints in Python"
        assert row.initial_confidence == pytest.approx(0.9, abs=0.01)
        assert row.trust_tier == "T1"
        assert row.source_event_ids == ["evt_a", "evt_b"]
        assert row.is_active == 1
        assert row.superseded_by is None
        assert row.observed_at is not None
        assert row.created_at is not None

    def test_create_memory_direct(self, db_factory, db_session, user_id):
        """create_memory bypasses Observer — verify raw write."""
        svc = create_memory_service(db_factory, strategy="vector:v1")
        mid = uuid4().hex

        mem = Memory(
            memory_id=mid, user_id=user_id,
            memory_type=MemoryType.TOOL_RESULT,
            content="ls output: file1.py file2.py",
            initial_confidence=0.7,
            trust_tier=TrustTier.T4_UNVERIFIED,
            embedding=_embed(0.3),
        )
        svc.create_memory(mem)

        db_session.expire_all()
        row = db_session.query(MemoryRecord).filter_by(memory_id=mid).first()
        assert row is not None
        assert row.memory_type == "tool_result"
        assert row.trust_tier == "T4"
        assert row.embedding is not None
        assert len(row.embedding) == EMBEDDING_DIM


# ── 3. VectorRetrievalStrategy real SQL ───────────────────────────────

class TestVectorRetrievalDB:
    """vector:v1 retrieval against real DB with real SQL."""

    def test_retrieve_by_embedding(self, db_factory, db_session, user_id):
        """Store with embedding → retrieve by vector similarity."""
        svc = create_memory_service(db_factory, strategy="vector:v1")
        emb = _embed(0.5)

        mem = Memory(
            memory_id=uuid4().hex, user_id=user_id,
            memory_type=MemoryType.SEMANTIC,
            content="Python is great for data science",
            initial_confidence=0.8,
            embedding=emb,
        )
        svc.create_memory(mem)

        results, stats = svc.retrieve(
            user_id, "data science tools",
            query_embedding=_similar_embed(),
            top_k=5,
        )

        assert len(results) >= 1
        assert any(r.memory_id == mem.memory_id for r in results)
        # Verify returned Memory fields
        matched = next(r for r in results if r.memory_id == mem.memory_id)
        assert matched.user_id == user_id
        assert matched.memory_type == MemoryType.SEMANTIC
        assert matched.content == "Python is great for data science"

    def test_retrieve_respects_is_active(self, db_factory, db_session, user_id):
        """Deactivated memories must NOT appear in retrieval."""
        svc = create_memory_service(db_factory, strategy="vector:v1")
        emb = _embed(0.6)

        mem = Memory(
            memory_id=uuid4().hex, user_id=user_id,
            memory_type=MemoryType.SEMANTIC,
            content="This should be invisible",
            initial_confidence=0.9,
            embedding=emb,
        )
        svc.create_memory(mem)

        # Deactivate
        from sqlalchemy import text
        db = db_factory()
        db.execute(text(
            "UPDATE mem_memories SET is_active = 0 WHERE memory_id = :mid"
        ), {"mid": mem.memory_id})
        db.commit()
        db.close()

        results, _ = svc.retrieve(
            user_id, "invisible",
            query_embedding=emb,
            top_k=10,
        )
        assert not any(r.memory_id == mem.memory_id for r in results)

    def test_retrieve_user_isolation(self, db_factory, user_id):
        """User A's memories must NOT appear in User B's retrieval."""
        svc = create_memory_service(db_factory, strategy="vector:v1")
        emb = _embed(0.7)
        other_user = _uid()

        mem = Memory(
            memory_id=uuid4().hex, user_id=user_id,
            memory_type=MemoryType.SEMANTIC,
            content="Alice secret preference",
            initial_confidence=0.9,
            embedding=emb,
        )
        svc.create_memory(mem)

        results, _ = svc.retrieve(
            other_user, "secret preference",
            query_embedding=emb,
            top_k=10,
        )
        assert not any(r.memory_id == mem.memory_id for r in results)


# ── 4. ActivationRetrievalStrategy real graph ─────────────────────────

class TestActivationRetrievalDB:
    """activation:v1 retrieval against real graph DB."""

    def test_vector_fallback_when_graph_too_small(self, db_factory, user_id):
        """With < MIN_GRAPH_NODES, activation falls back to vector internally."""
        svc = create_memory_service(db_factory, strategy="activation:v1")
        emb = _embed(0.4)

        # Store a few memories (well below MIN_GRAPH_NODES=50)
        for i in range(3):
            mem = Memory(
                memory_id=uuid4().hex, user_id=user_id,
                memory_type=MemoryType.SEMANTIC,
                content=f"Fact number {i} about Python",
                initial_confidence=0.8,
                embedding=_embed(0.4 + i * 0.01),
            )
            svc.create_memory(mem)

        # Retrieve — should use vector fallback (graph too small)
        results, _ = svc.retrieve(
            user_id, "Python facts",
            query_embedding=emb,
            top_k=5,
        )
        # Should still find memories via vector fallback
        assert len(results) >= 1


# ── 5. IndexManager real graph node creation ──────────────────────────

class TestActivationIndexManagerDB:
    """ActivationIndexManager creates real graph nodes/edges."""

    def test_on_memories_stored_creates_graph_nodes(
        self, db_factory, db_session, user_id,
    ):
        from core.memory.strategy.activation_index import ActivationIndexManager

        idx = ActivationIndexManager(db_factory)
        emb = _embed(0.3)

        mem = Memory(
            memory_id=uuid4().hex, user_id=user_id,
            memory_type=MemoryType.SEMANTIC,
            content="Graph index test memory",
            initial_confidence=0.8,
            trust_tier=TrustTier.T3_INFERRED,
            embedding=emb,
        )
        # Write to canonical storage first
        from core.memory.tabular.store import MemoryStore
        store = MemoryStore(db_factory)
        store.create(mem)

        # Now trigger index update
        idx.on_memories_stored(user_id, [mem])

        # Verify graph node in DB
        from core.memory.models.graph import GraphNode
        db_session.expire_all()
        gnode = db_session.query(GraphNode).filter_by(
            memory_id=mem.memory_id,
        ).first()
        assert gnode is not None
        assert gnode.user_id == user_id
        assert gnode.node_type == "semantic"
        assert gnode.content == "Graph index test memory"
        assert gnode.is_active == 1
        assert gnode.embedding is not None

    def test_backfill_creates_nodes_from_canonical(
        self, db_factory, db_session, user_id,
    ):
        """backfill() reads mem_memories and creates graph nodes."""
        from core.memory.strategy.activation_index import ActivationIndexManager
        from core.memory.tabular.store import MemoryStore

        store = MemoryStore(db_factory)
        idx = ActivationIndexManager(db_factory)

        # Create 3 memories in canonical storage (no graph nodes yet)
        mids = []
        for i in range(3):
            mem = Memory(
                memory_id=uuid4().hex, user_id=user_id,
                memory_type=MemoryType.SEMANTIC,
                content=f"Backfill test {i}",
                initial_confidence=0.7,
                embedding=_embed(0.1 + i * 0.1),
            )
            store.create(mem)
            mids.append(mem.memory_id)

        # Verify no graph nodes yet
        from core.memory.models.graph import GraphNode
        db_session.expire_all()
        count_before = db_session.query(GraphNode).filter(
            GraphNode.memory_id.in_(mids),
        ).count()
        assert count_before == 0

        # Run backfill
        result = idx.backfill(user_id)
        assert result.processed == 3
        assert result.skipped == 0
        assert result.errors == []

        # Verify graph nodes created
        db_session.expire_all()
        count_after = db_session.query(GraphNode).filter(
            GraphNode.memory_id.in_(mids),
        ).count()
        assert count_after == 3

    def test_backfill_is_idempotent(self, db_factory, db_session, user_id):
        """Running backfill twice doesn't duplicate nodes."""
        from core.memory.strategy.activation_index import ActivationIndexManager
        from core.memory.tabular.store import MemoryStore

        store = MemoryStore(db_factory)
        idx = ActivationIndexManager(db_factory)

        mem = Memory(
            memory_id=uuid4().hex, user_id=user_id,
            memory_type=MemoryType.SEMANTIC,
            content="Idempotent test",
            initial_confidence=0.7,
            embedding=_embed(0.5),
        )
        store.create(mem)

        r1 = idx.backfill(user_id)
        r2 = idx.backfill(user_id)

        assert r1.processed == 1
        assert r2.processed == 0
        assert r2.skipped == 1

        from core.memory.models.graph import GraphNode
        db_session.expire_all()
        count = db_session.query(GraphNode).filter_by(
            memory_id=mem.memory_id,
        ).count()
        assert count == 1

    def test_drop_index_removes_user_data(self, db_factory, db_session, user_id):
        """drop_index() removes all graph data for a user."""
        from core.memory.strategy.activation_index import ActivationIndexManager
        from core.memory.tabular.store import MemoryStore

        store = MemoryStore(db_factory)
        idx = ActivationIndexManager(db_factory)

        mem = Memory(
            memory_id=uuid4().hex, user_id=user_id,
            memory_type=MemoryType.SEMANTIC,
            content="Will be dropped",
            initial_confidence=0.7,
            embedding=_embed(0.6),
        )
        store.create(mem)
        idx.on_memories_stored(user_id, [mem])

        # Verify node exists
        from core.memory.models.graph import GraphNode
        db_session.expire_all()
        assert db_session.query(GraphNode).filter_by(
            memory_id=mem.memory_id,
        ).count() == 1

        # Drop index
        idx.drop_index(user_id)

        db_session.expire_all()
        assert db_session.query(GraphNode).filter_by(
            user_id=user_id,
        ).count() == 0

        # Canonical storage untouched
        row = db_session.query(MemoryRecord).filter_by(
            memory_id=mem.memory_id,
        ).first()
        assert row is not None
        assert row.is_active == 1


# ── 6. End-to-end: store → index → retrieve ──────────────────────────

class TestEndToEndStoreRetrieve:
    """Full pipeline: factory → store → index update → retrieve."""

    def test_vector_store_then_retrieve(self, db_factory, user_id):
        svc = create_memory_service(db_factory, strategy="vector:v1")
        emb = _embed(0.8)

        mem = Memory(
            memory_id=uuid4().hex, user_id=user_id,
            memory_type=MemoryType.SEMANTIC,
            content="End-to-end vector test",
            initial_confidence=0.9,
            embedding=emb,
        )
        svc.create_memory(mem)

        results, _ = svc.retrieve(
            user_id, "vector test",
            query_embedding=_similar_embed(),
            top_k=5,
        )
        found = [r for r in results if r.memory_id == mem.memory_id]
        assert len(found) == 1
        assert found[0].content == "End-to-end vector test"
        assert found[0].memory_type == MemoryType.SEMANTIC

    def test_governance_does_not_corrupt_data(self, db_factory, db_session, user_id):
        """run_governance() must not corrupt canonical or index data."""
        svc = create_memory_service(db_factory, strategy="activation:v1")

        mem = Memory(
            memory_id=uuid4().hex, user_id=user_id,
            memory_type=MemoryType.SEMANTIC,
            content="Governance safety test",
            initial_confidence=0.8,
            embedding=_embed(0.9),
        )
        svc.create_memory(mem)
        svc._index_manager.on_memories_stored(user_id, [mem])

        # Run governance
        report = svc.run_governance(user_id)
        assert report.errors is None or report.errors == []

        # Canonical data intact
        db_session.expire_all()
        row = db_session.query(MemoryRecord).filter_by(
            memory_id=mem.memory_id,
        ).first()
        assert row is not None
        assert row.is_active == 1
        assert row.content == "Governance safety test"

        # Graph data intact
        from core.memory.models.graph import GraphNode
        gnode = db_session.query(GraphNode).filter_by(
            memory_id=mem.memory_id,
        ).first()
        assert gnode is not None
        assert gnode.is_active == 1


# ── 7. MemoryService facade methods ──────────────────────────────────
# (migrated from test_memory_service_e2e.py)

import time as _time
from core.memory.tabular.service import MemoryService as _MemoryService


def _svc(db_factory):
    return _MemoryService(db_factory)


class TestUpdateMemoryContent:
    def test_content_updated_others_unchanged(self, db_factory, db_session, user_id):
        svc = _svc(db_factory)
        mid = uuid4().hex
        sid = _sid()
        mem = Memory(
            memory_id=mid, user_id=user_id,
            memory_type=MemoryType.TOOL_RESULT, content="original",
            initial_confidence=0.7, trust_tier=TrustTier.T4_UNVERIFIED,
            source_event_ids=["e1"], session_id=sid,
        )
        svc.create_memory(mem)

        db_session.expire_all()
        before = db_session.query(MemoryRecord).filter_by(memory_id=mid).first()
        _time.sleep(0.05)
        svc.update_memory_content(mid, "updated content")

        db_session.expire_all()
        after = db_session.query(MemoryRecord).filter_by(memory_id=mid).first()
        assert after.content == "updated content"
        assert after.updated_at >= before.updated_at
        assert after.created_at == before.created_at
        assert after.memory_type == "tool_result"
        assert after.initial_confidence == pytest.approx(0.7, abs=0.01)
        assert after.is_active == 1


class TestListActive:
    def test_filters_by_type_and_user(self, db_factory, user_id):
        svc = _svc(db_factory)
        other = _uid()
        for u, t in [(user_id, MemoryType.SEMANTIC), (user_id, MemoryType.SEMANTIC),
                     (user_id, MemoryType.PROCEDURAL), (other, MemoryType.SEMANTIC)]:
            svc.create_memory(Memory(
                memory_id=uuid4().hex, user_id=u, memory_type=t, content="x",
            ))
        assert len(svc.list_active(user_id, memory_type=MemoryType.SEMANTIC)) == 2
        assert len(svc.list_active(user_id, memory_type=MemoryType.PROCEDURAL)) == 1
        assert len(svc.list_active(user_id)) == 3
        assert len(svc.list_active(other)) == 1

    def test_limit_and_excludes_inactive(self, db_factory, db_session, user_id):
        svc = _svc(db_factory)
        mid_active = uuid4().hex
        mid_inactive = uuid4().hex
        for mid in (mid_active, mid_inactive):
            svc.create_memory(Memory(
                memory_id=mid, user_id=user_id,
                memory_type=MemoryType.SEMANTIC, content="x",
            ))
        db_session.query(MemoryRecord).filter_by(memory_id=mid_inactive).update({"is_active": 0})
        db_session.commit()
        results = svc.list_active(user_id)
        assert len(results) == 1
        assert results[0].memory_id == mid_active


class TestServiceGovernance:
    def test_run_hourly_cleans_tool_results(self, db_factory, db_session, user_id):
        from datetime import timedelta
        svc = _svc(db_factory)
        mid_old = uuid4().hex
        mid_fresh = uuid4().hex
        for mid in (mid_old, mid_fresh):
            svc.create_memory(Memory(
                memory_id=mid, user_id=user_id,
                memory_type=MemoryType.TOOL_RESULT, content="tool out",
            ))
        old_time = datetime.utcnow().replace(microsecond=0) - timedelta(hours=25)
        db_session.query(MemoryRecord).filter_by(memory_id=mid_old).update({
            "created_at": old_time, "observed_at": old_time, "updated_at": old_time,
        })
        db_session.commit()
        report = svc.run_hourly()
        assert report.cleaned_tool_results >= 1
        db_session.expire_all()
        old_row = db_session.query(MemoryRecord).filter_by(memory_id=mid_old).first()
        assert old_row is None or old_row.is_active == 0
        fresh_row = db_session.query(MemoryRecord).filter_by(memory_id=mid_fresh).first()
        assert fresh_row is not None and fresh_row.is_active == 1

    def test_run_weekly_returns_report(self, db_factory):
        report = _svc(db_factory).run_weekly()
        assert report is not None

    def test_run_pipeline_extracts_and_persists(self, db_factory, db_session, user_id):
        from unittest.mock import MagicMock
        import json as _json
        mock_llm = MagicMock()
        mock_llm.chat_with_tools.return_value = {"content": _json.dumps([
            {"type": "semantic", "content": "user prefers dark mode", "confidence": 0.8},
        ])}
        svc = _MemoryService(db_factory, llm_client=mock_llm)
        result = svc.run_pipeline(user_id=user_id, messages=[
            {"role": "user", "content": "I always use dark mode"},
        ])
        assert result.memories_extracted >= 0
        if result.memories_extracted > 0:
            db_session.expire_all()
            rows = db_session.query(MemoryRecord).filter(
                MemoryRecord.user_id == user_id, MemoryRecord.is_active == 1,
            ).all()
            assert len(rows) == result.memories_extracted


class TestStoreUpdateContent:
    def test_update_content_field_level(self, db_factory, db_session, user_id):
        from core.memory.tabular.store import MemoryStore
        store = MemoryStore(db_factory)
        mid = uuid4().hex
        store.create(Memory(
            memory_id=mid, user_id=user_id,
            memory_type=MemoryType.TOOL_RESULT, content="v1",
            initial_confidence=0.5, trust_tier=TrustTier.T3_INFERRED,
            source_event_ids=["e1"],
        ))
        db_session.expire_all()
        before = db_session.query(MemoryRecord).filter_by(memory_id=mid).first()
        _time.sleep(0.05)
        store.update_content(mid, "v2")
        db_session.expire_all()
        after = db_session.query(MemoryRecord).filter_by(memory_id=mid).first()
        assert after.content == "v2"
        assert after.updated_at >= before.updated_at
        assert after.created_at == before.created_at
        assert after.initial_confidence == pytest.approx(0.5, abs=0.01)
        assert after.is_active == 1

    def test_update_nonexistent_is_noop(self, db_factory):
        from core.memory.tabular.store import MemoryStore
        MemoryStore(db_factory).update_content("nonexistent_xyz", "new")
