"""Critical path verification — ensures DB features actually execute (not fallback).

This test verifies that:
1. Vector search (L2_DISTANCE) actually runs on DB
2. Fulltext search (MATCH AGAINST) actually runs on DB  
3. Sandbox branch operations actually run on DB
4. Contradiction detection actually runs on DB

All verifications use explain stats — if stats show error, the feature fell back.
"""

import json
from datetime import datetime
from uuid import uuid4

import pytest

from core.memory.store import MemoryStore
from core.memory.retriever import MemoryRetriever
from core.memory.typed_observer import TypedObserver
from core.memory.sandbox import MemorySandbox
from core.memory.types import Memory, MemoryType


def _uid():
    return f"path_verify_{uuid4().hex[:8]}"


def _embed(text: str) -> list[float]:
    return [hash(text) % 100 / 100.0] * 1536


class TestCriticalPathVerification:
    """Verify all DB-dependent features actually execute (not fallback)."""

    @pytest.fixture
    def db_factory(self):
        from api.database import SessionLocal
        return SessionLocal

    @pytest.fixture
    def cleanup_memories(self, db_factory):
        memory_ids = []
        yield memory_ids
        if memory_ids:
            from sqlalchemy import text
            db = db_factory()
            try:
                db.execute(
                    text("DELETE FROM memories WHERE memory_id IN :ids"),
                    {"ids": tuple(memory_ids)}
                )
                db.commit()
            finally:
                db.close()

    def test_vector_search_actually_executes(self, db_factory, cleanup_memories):
        """L2_DISTANCE vector search runs on DB, not fallback."""
        store = MemoryStore(db_factory)
        retriever = MemoryRetriever(db_factory)
        user_id = _uid()

        # Create memory with embedding
        mem = Memory(
            memory_id=f"vec_{uuid4().hex}",
            user_id=user_id,
            memory_type=MemoryType.SEMANTIC,
            content="Vector search verification memory",
            confidence=0.9,
            embedding=[0.1] * 1536,
            observed_at=datetime.utcnow(),
        )
        cleanup_memories.append(mem.memory_id)
        store.create(mem)

        # Query with embedding
        results, stats = retriever.retrieve(
            user_id=user_id,
            query_text="vector",
            session_id="test",
            query_embedding=[0.1] * 1536,
            explain=True,
        )

        # CRITICAL: verify vector search actually ran
        assert stats.vector_attempted is True, "Vector search should have been attempted"
        assert stats.vector_error is None, f"Vector search failed with: {stats.vector_error}"
        print(f"✓ Vector search executed: {stats.phase2_candidates} candidates in {stats.phase2_ms:.1f}ms")

    def test_fulltext_search_actually_executes(self, db_factory, cleanup_memories):
        """MATCH AGAINST fulltext search runs on DB, not fallback."""
        store = MemoryStore(db_factory)
        retriever = MemoryRetriever(db_factory)
        user_id = _uid()

        # Create memory with searchable content
        mem = Memory(
            memory_id=f"ft_{uuid4().hex}",
            user_id=user_id,
            memory_type=MemoryType.SEMANTIC,
            content="Python programming language expertise",
            confidence=0.9,
            observed_at=datetime.utcnow(),
        )
        cleanup_memories.append(mem.memory_id)
        store.create(mem)

        # Query with keyword
        results, stats = retriever.retrieve(
            user_id=user_id,
            query_text="Python programming",
            session_id="test",
            explain=True,
        )

        # CRITICAL: verify keyword search actually ran
        assert stats.keyword_attempted is True, "Keyword search should have been attempted"
        # Note: keyword_hit may be False if no fulltext index, but error should be None if it ran
        if stats.keyword_error:
            pytest.fail(f"Fulltext search failed with: {stats.keyword_error}")
        print(f"✓ Fulltext search executed: hit={stats.keyword_hit}, {stats.phase1_candidates} candidates in {stats.phase1_ms:.1f}ms")

    def test_sandbox_branch_actually_executes(self, db_factory, cleanup_memories):
        """Sandbox branch create/drop runs on DB, not fallback."""
        store = MemoryStore(db_factory)
        sandbox = MemorySandbox(db_factory)
        user_id = _uid()

        # Create base memory
        mem = Memory(
            memory_id=f"base_{uuid4().hex}",
            user_id=user_id,
            memory_type=MemoryType.PROFILE,
            content="Base memory for sandbox test",
            confidence=0.8,
            embedding=[0.5] * 1536,
            observed_at=datetime.utcnow(),
        )
        cleanup_memories.append(mem.memory_id)
        store.create(mem)

        # New memory to validate
        new_mem = Memory(
            memory_id=f"new_{uuid4().hex}",
            user_id=user_id,
            memory_type=MemoryType.PROFILE,
            content="New memory to validate in sandbox",
            confidence=0.9,
            embedding=[0.5] * 1536,
            observed_at=datetime.utcnow(),
        )

        # Validate with explain
        result, stats = sandbox.validate_memories(
            user_id=user_id,
            new_memories=[new_mem],
            query_text="test",
            query_embedding=[0.5] * 1536,
            explain=True,
        )

        # CRITICAL: verify sandbox actually ran
        assert stats.enabled is True, "Sandbox should be enabled"
        assert stats.error is None, f"Sandbox failed with: {stats.error}"
        assert stats.validated is True, "Sandbox should have validated"
        print(f"✓ Sandbox branch executed: branch={stats.branch_name}, {stats.total_ms:.1f}ms")

    def test_contradiction_detection_actually_executes(self, db_factory, cleanup_memories):
        """Contradiction detection L2_DISTANCE runs on DB, not fallback."""
        store = MemoryStore(db_factory)
        user_id = _uid()

        # Create existing memory
        old_mem = Memory(
            memory_id=f"old_{uuid4().hex}",
            user_id=user_id,
            memory_type=MemoryType.PROFILE,
            content="User prefers tabs",
            confidence=0.8,
            embedding=[0.5] * 1536,
            observed_at=datetime.utcnow(),
        )
        cleanup_memories.append(old_mem.memory_id)
        store.create(old_mem)

        # Observer with real DB
        observer = TypedObserver(
            store=store,
            llm_client=None,
            embed_fn=lambda x: [0.5] * 1536,  # Same embedding → should find contradiction
            db_factory=db_factory,
        )

        # Write contradicting memory with explain
        new_mem, stats = observer.observe_explicit(
            user_id=user_id,
            content="User prefers spaces",
            memory_type=MemoryType.PROFILE,
            confidence=0.9,
            explain=True,
        )
        cleanup_memories.append(new_mem.memory_id)

        # CRITICAL: verify contradiction detection actually ran
        assert stats.checked is True, "Contradiction check should have run"
        assert stats.error is None, f"Contradiction detection failed with: {stats.error}"
        # Should have found the contradiction (same embedding, different content)
        assert stats.found is True, "Should have found contradiction"
        assert stats.superseded_id == old_mem.memory_id, "Should have superseded old memory"
        print(f"✓ Contradiction detection executed: found={stats.found}, superseded={stats.superseded_id}, {stats.query_ms:.1f}ms")

    def test_all_paths_summary(self, db_factory, cleanup_memories):
        """Summary test: run all critical paths and report."""
        store = MemoryStore(db_factory)
        retriever = MemoryRetriever(db_factory)
        sandbox = MemorySandbox(db_factory)
        user_id = _uid()

        # Setup
        mem = Memory(
            memory_id=f"sum_{uuid4().hex}",
            user_id=user_id,
            memory_type=MemoryType.SEMANTIC,
            content="Summary test memory for all paths",
            confidence=0.9,
            embedding=[0.3] * 1536,
            observed_at=datetime.utcnow(),
        )
        cleanup_memories.append(mem.memory_id)
        store.create(mem)

        print("\n" + "="*60)
        print("CRITICAL PATH VERIFICATION SUMMARY")
        print("="*60)

        # 1. Vector search
        _, stats = retriever.retrieve(
            user_id=user_id, query_text="test", session_id="s",
            query_embedding=[0.3] * 1536, explain=True,
        )
        vec_ok = stats.vector_attempted and stats.vector_error is None
        print(f"[{'✓' if vec_ok else '✗'}] Vector Search (L2_DISTANCE): {'OK' if vec_ok else stats.vector_error}")

        # 2. Fulltext search
        _, stats = retriever.retrieve(
            user_id=user_id, query_text="Summary test", session_id="s",
            explain=True,
        )
        ft_ok = stats.keyword_attempted and stats.keyword_error is None
        print(f"[{'✓' if ft_ok else '✗'}] Fulltext Search (MATCH AGAINST): {'OK' if ft_ok else stats.keyword_error}")

        # 3. Sandbox
        new_mem = Memory(
            memory_id=f"sbox_{uuid4().hex}",
            user_id=user_id,
            memory_type=MemoryType.PROFILE,
            content="Sandbox test",
            confidence=0.9,
            embedding=[0.3] * 1536,
            observed_at=datetime.utcnow(),
        )
        _, stats = sandbox.validate_memories(
            user_id=user_id, new_memories=[new_mem],
            query_text="test", query_embedding=[0.3] * 1536,
            explain=True,
        )
        sb_ok = stats.enabled and stats.error is None
        print(f"[{'✓' if sb_ok else '✗'}] Sandbox Branch: {'OK' if sb_ok else stats.error}")

        # 4. Contradiction detection
        observer = TypedObserver(
            store=store, llm_client=None,
            embed_fn=lambda x: [0.3] * 1536,
            db_factory=db_factory,
        )
        _, stats = observer.observe_explicit(
            user_id=user_id,
            content="Contradiction test",
            memory_type=MemoryType.PROFILE,
            confidence=0.9,
            explain=True,
        )
        cd_ok = stats.checked and stats.error is None
        print(f"[{'✓' if cd_ok else '✗'}] Contradiction Detection: {'OK' if cd_ok else stats.error}")

        print("="*60)

        # All must pass
        assert vec_ok, "Vector search failed"
        assert ft_ok, "Fulltext search failed"
        assert sb_ok, "Sandbox failed"
        assert cd_ok, "Contradiction detection failed"
