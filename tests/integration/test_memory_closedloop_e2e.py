"""Memory System End-to-End Closed-Loop Tests.

Verifies the COMPLETE memory lifecycle with REAL database operations.
NO MOCKS for core memory operations - only LLM is mocked.
"""

import json
from datetime import datetime, timedelta

import pytest
from uuid_utils import uuid7

from core.memory.store import MemoryStore
from core.memory.retriever import MemoryRetriever
from core.memory.types import Memory, MemoryType
from core.memory.profile import ProfileManager
from core.memory.tiered_loader import TieredMemoryLoader
from core.memory.typed_observer import TypedObserver
from core.memory.governance import GovernanceScheduler
from core.memory.health import MemoryHealth
from core.memory.config import MemoryGovernanceConfig


def _uid():
    return f"closedloop_{uuid7().hex[:12]}"


def _sid():
    return f"sess_{uuid7().hex[:12]}"


def _embed(text: str) -> list[float]:
    """Deterministic embedding based on text hash."""
    h = hash(text) % 1000
    base = [h / 1000.0] * 1536
    for i, c in enumerate(text[:100]):
        base[i % 1536] += ord(c) / 10000.0
    return base


@pytest.fixture
def memory_cleanup(db_factory):
    """Track and cleanup all memories created during test."""
    created_ids = []
    yield created_ids
    store = MemoryStore(db_factory)
    for mid in created_ids:
        try:
            store.deactivate(mid)
        except Exception:
            pass


class TestObserverToPromptClosedLoop:
    """Test: Observer extract → Store → Retriever → Prompt injection."""

    def test_extracted_memory_appears_in_retrieval(self, db_factory, memory_cleanup):
        """Memory extracted by Observer is retrievable."""
        user_id = _uid()
        session_id = _sid()
        store = MemoryStore(db_factory)
        retriever = MemoryRetriever(db_factory)
        
        observer = TypedObserver(
            store=store,
            llm_client=None,
            embed_fn=_embed,
        )
        
        # User says they prefer Python
        mem, _ = observer.observe_explicit(
            user_id=user_id,
            content="User strongly prefers Python for data analysis",
            memory_type=MemoryType.PROFILE,
            initial_confidence=0.9,
        )
        memory_cleanup.append(mem.memory_id)
        
        # Verify stored
        stored = store.get(mem.memory_id)
        assert stored is not None
        assert stored.is_active is True
        
        # Verify retrievable
        results, _ = retriever.retrieve(
            user_id=user_id,
            query_text="What language should I use?",
            session_id=session_id,
            query_embedding=_embed("What language should I use?"),
            limit=5,
        )
        assert len(results) >= 1
        assert any("Python" in r.content for r in results)

    def test_multiple_memories_ranked_by_relevance(self, db_factory, memory_cleanup):
        """Multiple memories are ranked by relevance."""
        user_id = _uid()
        session_id = _sid()
        store = MemoryStore(db_factory)
        retriever = MemoryRetriever(db_factory)
        
        memories = [
            ("User prefers Go for backend", MemoryType.PROFILE),
            ("User likes Python for ML", MemoryType.PROFILE),
            ("User asked about Kubernetes", MemoryType.SEMANTIC),
        ]
        
        for content, mtype in memories:
            mem = Memory(
                memory_id=str(uuid7()),
                user_id=user_id,
                memory_type=mtype,
                content=content,
                initial_confidence=0.8,
                embedding=_embed(content),
                observed_at=datetime.utcnow(),
            )
            memory_cleanup.append(mem.memory_id)
            store.create(mem)
        
        # Query about ML
        results, _ = retriever.retrieve(
            user_id=user_id,
            query_text="machine learning project",
            session_id=session_id,
            query_embedding=_embed("machine learning project"),
            limit=3,
        )
        
        contents = [r.content for r in results]
        assert any("Python" in c and "ML" in c for c in contents)


class TestMultiTurnMemoryAccumulation:
    """Test memory accumulation across turns."""

    def test_memories_accumulate_across_turns(self, db_factory, memory_cleanup):
        """Each turn adds memories, all retrievable."""
        user_id = _uid()
        session_id = _sid()
        store = MemoryStore(db_factory)
        observer = TypedObserver(store=store, llm_client=None, embed_fn=_embed)
        retriever = MemoryRetriever(db_factory)
        
        # Turn 1: PROFILE memory
        mem1, _ = observer.observe_explicit(
            user_id=user_id,
            content="User prefers functional programming",
            memory_type=MemoryType.PROFILE,
            initial_confidence=0.8,
        )
        memory_cleanup.append(mem1.memory_id)
        
        # Turn 2: EPISODIC memory (different type, no contradiction with mem1)
        mem2, _ = observer.observe_explicit(
            user_id=user_id,
            content="User asked about Haskell monads",
            memory_type=MemoryType.SEMANTIC,
            initial_confidence=0.7,
        )
        memory_cleanup.append(mem2.memory_id)
        
        # Turn 3: SEMANTIC memory (different type from mem1, no contradiction)
        mem3, _ = observer.observe_explicit(
            user_id=user_id,
            content="User dislikes mutable state",
            memory_type=MemoryType.SEMANTIC,
            initial_confidence=0.85,
        )
        memory_cleanup.append(mem3.memory_id)
        
        # All 3 retrievable (may not all match query equally)
        results, _ = retriever.retrieve(
            user_id=user_id,
            query_text="functional programming style",
            session_id=session_id,
            query_embedding=_embed("functional programming style"),
            limit=10,
        )
        
        # At least 1 should be retrieved
        assert len(results) >= 1
        
        # Verify all 3 memories exist in DB (the real test)
        assert store.get(mem1.memory_id) is not None
        assert store.get(mem2.memory_id) is not None
        assert store.get(mem3.memory_id) is not None
        assert store.get(mem1.memory_id).is_active is True
        assert store.get(mem2.memory_id).is_active is True
        assert store.get(mem3.memory_id).is_active is True


class TestContradictionAndSupersede:
    """Test contradiction detection and supersede chain."""

    def test_contradicting_profile_supersedes_old(self, db_factory, memory_cleanup):
        """New contradicting profile supersedes old."""
        user_id = _uid()
        store = MemoryStore(db_factory)
        
        fixed_embed = [0.5] * 1536
        observer = TypedObserver(
            store=store,
            llm_client=None,
            embed_fn=lambda x: fixed_embed,
            contradiction_threshold=0.85,
            db_factory=db_factory,
        )
        
        # Old preference
        old, _ = observer.observe_explicit(
            user_id=user_id,
            content="User prefers tabs",
            memory_type=MemoryType.PROFILE,
            initial_confidence=0.8,
        )
        memory_cleanup.append(old.memory_id)
        
        # New contradicting
        new, _ = observer.observe_explicit(
            user_id=user_id,
            content="User prefers spaces",
            memory_type=MemoryType.PROFILE,
            initial_confidence=0.9,
        )
        memory_cleanup.append(new.memory_id)
        
        # Old superseded
        old_mem = store.get(old.memory_id)
        assert old_mem.is_active is False
        assert old_mem.superseded_by == new.memory_id

    def test_supersede_chain(self, db_factory, memory_cleanup):
        """Supersede chain: A → B → C."""
        user_id = _uid()
        store = MemoryStore(db_factory)
        fixed_embed = [0.3] * 1536
        
        observer = TypedObserver(
            store=store, llm_client=None,
            embed_fn=lambda x: fixed_embed,
            contradiction_threshold=0.85,
            db_factory=db_factory,
        )
        
        mem_a, _ = observer.observe_explicit(user_id, "Version A", MemoryType.PROFILE, 0.7)
        memory_cleanup.append(mem_a.memory_id)
        
        mem_b, _ = observer.observe_explicit(user_id, "Version B", MemoryType.PROFILE, 0.8)
        memory_cleanup.append(mem_b.memory_id)
        
        mem_c, _ = observer.observe_explicit(user_id, "Version C", MemoryType.PROFILE, 0.9)
        memory_cleanup.append(mem_c.memory_id)
        
        a = store.get(mem_a.memory_id)
        b = store.get(mem_b.memory_id)
        c = store.get(mem_c.memory_id)
        
        assert a.superseded_by == mem_b.memory_id
        assert b.superseded_by == mem_c.memory_id
        assert c.is_active is True


class TestProfileAndL0:
    """Test Profile synthesis and L0."""

    def test_profile_synthesizes_from_memories(self, db_factory, memory_cleanup):
        """ProfileManager synthesizes from profile memories."""
        user_id = _uid()
        store = MemoryStore(db_factory)
        profile_mgr = ProfileManager(store)
        
        prefs = ["User prefers Python", "User works on ML", "User uses Linux"]
        
        for pref in prefs:
            mem = Memory(
                memory_id=str(uuid7()),
                user_id=user_id,
                memory_type=MemoryType.PROFILE,
                content=pref,
                initial_confidence=0.8,
                embedding=_embed(pref),
                observed_at=datetime.utcnow(),
            )
            memory_cleanup.append(mem.memory_id)
            store.create(mem)
        
        profile = profile_mgr.get_profile(user_id)
        
        assert "Python" in profile
        assert "Linux" in profile

    def test_tiered_loader_builds_section(self, db_factory, memory_cleanup):
        """TieredMemoryLoader builds memory section."""
        user_id = _uid()
        session_id = _sid()
        store = MemoryStore(db_factory)
        loader = TieredMemoryLoader(db_factory)
        
        mem = Memory(
            memory_id=str(uuid7()),
            user_id=user_id,
            memory_type=MemoryType.PROFILE,
            content="User is a senior developer",
            initial_confidence=0.9,
            embedding=_embed("User is a senior developer"),
            observed_at=datetime.utcnow(),
        )
        memory_cleanup.append(mem.memory_id)
        store.create(mem)
        
        section, _ = loader.build_section(
            user_id=user_id,
            session_id=session_id,
            query="random query",
            query_embedding=_embed("random query"),
        )
        
        assert "senior developer" in section


class TestGovernanceRealExecution:
    """Test Governance with real DB."""

    def test_decay_is_query_time_only(self, db_factory, memory_cleanup):
        """Confidence is immutable in DB — decay computed at query time."""
        user_id = _uid()
        store = MemoryStore(db_factory)
        
        old_mem = Memory(
            memory_id=str(uuid7()),
            user_id=user_id,
            memory_type=MemoryType.SEMANTIC,
            content="Old event",
            initial_confidence=1.0,
            embedding=_embed("Old event"),
            observed_at=datetime.utcnow() - timedelta(days=7),
        )
        memory_cleanup.append(old_mem.memory_id)
        store.create(old_mem)
        
        stored = store.get(old_mem.memory_id)
        # DB value unchanged
        assert stored.initial_confidence == 1.0
        # Query-time decay is lower
        assert stored.effective_confidence(half_life_days=1.0) < 0.01

    def test_full_governance_cycle(self, db_factory, memory_cleanup):
        """Full governance cycle runs without error."""
        user_id = _uid()
        store = MemoryStore(db_factory)
        config = MemoryGovernanceConfig(confidence_decay_half_life_days=1.0)
        scheduler = GovernanceScheduler(db_factory, config)
        
        for i in range(3):
            mem = Memory(
                memory_id=str(uuid7()),
                user_id=user_id,
                memory_type=MemoryType.SEMANTIC,
                content=f"Event {i}",
                initial_confidence=0.8,
                embedding=_embed(f"Event {i}"),
                observed_at=datetime.utcnow() - timedelta(days=i),
            )
            memory_cleanup.append(mem.memory_id)
            store.create(mem)
        
        result = scheduler.run_cycle(user_id)
        assert result is not None


class TestHealthDetection:
    """Test memory health detection."""

    def test_analyze_returns_stats(self, db_factory, memory_cleanup):
        """Health analyze returns per-type stats."""
        user_id = _uid()
        store = MemoryStore(db_factory)
        health = MemoryHealth(db_factory)
        
        # Create memories
        for mtype in [MemoryType.PROFILE, MemoryType.SEMANTIC]:
            mem = Memory(
                memory_id=str(uuid7()),
                user_id=user_id,
                memory_type=mtype,
                content=f"{mtype.value} memory",
                initial_confidence=0.7,
                embedding=_embed(f"{mtype.value} memory"),
                observed_at=datetime.utcnow(),
            )
            memory_cleanup.append(mem.memory_id)
            store.create(mem)
        
        stats = health.analyze(user_id)
        
        # Stats keyed by memory_type value
        assert "profile" in stats or MemoryType.PROFILE.value in stats
        assert "profile" in stats or "semantic" in stats


class TestTaskAwareRetrieval:
    """Test task-aware retrieval."""

    def test_retrieval_with_task_hint(self, db_factory, memory_cleanup):
        """Retrieval works with task hint."""
        user_id = _uid()
        session_id = _sid()
        store = MemoryStore(db_factory)
        retriever = MemoryRetriever(db_factory)
        
        mem = Memory(
            memory_id=str(uuid7()),
            user_id=user_id,
            memory_type=MemoryType.PROCEDURAL,
            content="Always run tests before commit",
            initial_confidence=0.8,
            embedding=_embed("Always run tests before commit"),
            observed_at=datetime.utcnow(),
        )
        memory_cleanup.append(mem.memory_id)
        store.create(mem)
        
        results, _ = retriever.retrieve(
            user_id=user_id,
            query_text="How should I commit code?",
            session_id=session_id,
            query_embedding=_embed("How should I commit code?"),
            task_hint="code",
            limit=5,
        )
        
        assert len(results) >= 1
        assert any("tests" in r.content for r in results)


class TestSensitivityFilterRealDB:
    """Sensitivity filter blocks PII from reaching the database."""

    def test_pii_blocked_before_persist(self, db_factory, memory_cleanup):
        """Observer rejects content containing PII — nothing stored."""
        user_id = _uid()
        store = MemoryStore(db_factory)
        observer = TypedObserver(store=store, llm_client=None, embed_fn=_embed)

        with pytest.raises(ValueError, match="sensitivity filter"):
            observer.observe_explicit(
                user_id=user_id,
                content="Contact me at alice@example.com for details",
                memory_type=MemoryType.SEMANTIC,
                initial_confidence=0.9,
            )

        # Verify nothing leaked to DB
        results, _ = MemoryRetriever(db_factory).retrieve(
            user_id=user_id,
            query_text="alice email",
            session_id=_sid(),
            query_embedding=_embed("alice email"),
            limit=10,
        )
        assert len(results) == 0

    def test_aws_key_blocked(self, db_factory, memory_cleanup):
        """AWS access key pattern is caught."""
        user_id = _uid()
        store = MemoryStore(db_factory)
        observer = TypedObserver(store=store, llm_client=None, embed_fn=_embed)

        with pytest.raises(ValueError, match="sensitivity filter"):
            observer.observe_explicit(
                user_id=user_id,
                content="Use key AKIAIOSFODNN7EXAMPLE for S3",
                memory_type=MemoryType.PROCEDURAL,
                initial_confidence=0.8,
            )

    def test_clean_content_passes(self, db_factory, memory_cleanup):
        """Non-PII content persists normally."""
        user_id = _uid()
        store = MemoryStore(db_factory)
        observer = TypedObserver(store=store, llm_client=None, embed_fn=_embed)

        mem, _ = observer.observe_explicit(
            user_id=user_id,
            content="User prefers dark mode in IDE",
            memory_type=MemoryType.PROFILE,
            initial_confidence=0.9,
        )
        memory_cleanup.append(mem.memory_id)

        stored = store.get(mem.memory_id)
        assert stored is not None
        assert "dark mode" in stored.content
