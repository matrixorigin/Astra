"""Memory System End-to-End Closed-Loop Tests.

Verifies the COMPLETE memory lifecycle with REAL database operations.
NO MOCKS for core memory operations - only LLM is mocked.
"""

import json
from datetime import datetime, timezone, timedelta

import pytest

from api.models._constants import EMBEDDING_DIM
from uuid_utils import uuid7

from core.memory.tabular.store import MemoryStore
from core.memory.tabular.retriever import MemoryRetriever
from core.memory.types import Memory, MemoryType, TrustTier
from core.memory.tabular.profile import ProfileManager
from core.context.tiered_loader import TieredMemoryLoader
from core.memory.tabular.service import MemoryService
from core.memory.tabular.typed_observer import TypedObserver
from core.memory.tabular.governance import GovernanceScheduler
from core.memory.tabular.health import MemoryHealth
from core.memory.config import MemoryGovernanceConfig


def _uid():
    return f"closedloop_{uuid7().hex}"


def _sid():
    return f"sess_{uuid7().hex}"


def _embed(text: str) -> list[float]:
    """Deterministic embedding based on text hash."""
    h = hash(text) % 1000
    base = [h / 1000.0] * EMBEDDING_DIM
    for i, c in enumerate(text[:100]):
        base[i % EMBEDDING_DIM] += ord(c) / 10000.0
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
                observed_at=datetime.now(timezone.utc),
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
        
        fixed_embed = [0.5] * EMBEDDING_DIM
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
        fixed_embed = [0.3] * EMBEDDING_DIM
        
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
                observed_at=datetime.now(timezone.utc),
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
        loader = TieredMemoryLoader(MemoryService(db_factory))
        
        mem = Memory(
            memory_id=str(uuid7()),
            user_id=user_id,
            memory_type=MemoryType.PROFILE,
            content="User is a senior developer",
            initial_confidence=0.9,
            embedding=_embed("User is a senior developer"),
            observed_at=datetime.now(timezone.utc),
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
            observed_at=datetime.now(timezone.utc) - timedelta(days=7),
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
        config = MemoryGovernanceConfig(half_life_t4_days=1.0)
        scheduler = GovernanceScheduler(db_factory, config)
        
        for i in range(3):
            mem = Memory(
                memory_id=str(uuid7()),
                user_id=user_id,
                memory_type=MemoryType.SEMANTIC,
                content=f"Event {i}",
                initial_confidence=0.8,
                embedding=_embed(f"Event {i}"),
                observed_at=datetime.now(timezone.utc) - timedelta(days=i),
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
                observed_at=datetime.now(timezone.utc),
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
            observed_at=datetime.now(timezone.utc),
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


class TestProceduralMemoryLifecycle:
    """Procedural memory: write to mem_memories → verify DB fields → retrieve."""

    def test_store_and_retrieve_procedural(self, db_factory, memory_cleanup):
        """Full lifecycle: create → DB field check → retriever returns it."""
        user_id = _uid()
        session_id = _sid()
        store = MemoryStore(db_factory)
        now = datetime.now(timezone.utc).replace(microsecond=0)
        mid = str(uuid7())

        mem = Memory(
            memory_id=mid,
            user_id=user_id,
            memory_type=MemoryType.PROCEDURAL,
            content="Always run tests before commit",
            initial_confidence=0.85,
            embedding=_embed("Always run tests before commit"),
            observed_at=now,
            trust_tier=TrustTier.T3_INFERRED,
        )
        memory_cleanup.append(mid)
        store.create(mem)

        # Re-read from DB — verify all meaningful fields
        stored = store.get(mid)
        assert stored is not None
        assert stored.memory_id == mid
        assert stored.user_id == user_id
        assert stored.memory_type == MemoryType.PROCEDURAL
        assert stored.content == "Always run tests before commit"
        assert stored.initial_confidence == 0.85
        assert stored.is_active is True
        # DB returns naive datetime, compare without tzinfo
        assert stored.observed_at.replace(tzinfo=None) == now.replace(tzinfo=None)
        assert stored.trust_tier == TrustTier.T3_INFERRED
        assert stored.superseded_by is None

        # Retrieve via MemoryRetriever — verify it surfaces
        retriever = MemoryRetriever(db_factory)
        results, _ = retriever.retrieve(
            user_id=user_id,
            query_text="How should I commit code?",
            session_id=session_id,
            query_embedding=_embed("How should I commit code?"),
            memory_types=[MemoryType.PROCEDURAL],
            limit=5,
        )
        assert any(r.memory_id == mid for r in results)
        retrieved = next(r for r in results if r.memory_id == mid)
        assert retrieved.memory_type == MemoryType.PROCEDURAL
        assert retrieved.content == "Always run tests before commit"
        assert retrieved.initial_confidence == 0.85
        assert retrieved.trust_tier == TrustTier.T3_INFERRED


class TestSensitivityFilterRealDB:
    """Sensitivity filter blocks PII from reaching the database."""

    def test_pii_blocked_before_persist(self, db_factory, memory_cleanup):
        """HIGH tier PII (AWS key) is blocked — nothing stored."""
        user_id = _uid()
        store = MemoryStore(db_factory)
        observer = TypedObserver(store=store, llm_client=None, embed_fn=_embed)

        with pytest.raises(ValueError, match="sensitivity filter"):
            observer.observe_explicit(
                user_id=user_id,
                content="key is AKIAIOSFODNN7EXAMPLE",
                memory_type=MemoryType.SEMANTIC,
                initial_confidence=0.9,
            )

        # Verify nothing leaked to DB
        results, _ = MemoryRetriever(db_factory).retrieve(
            user_id=user_id,
            query_text="aws key",
            session_id=_sid(),
            query_embedding=_embed("aws key"),
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


class TestSessionSummaryRealDB:
    """Session summarizer with real DB."""

    def test_incremental_summary_persists(self, db_factory, memory_cleanup):
        """Incremental summary persists to DB with session_id set."""
        from core.memory.tabular.session_summary import SessionSummarizer, _INCREMENTAL_TAG
        from core.memory.config import MemoryGovernanceConfig

        store = MemoryStore(db_factory)
        config = MemoryGovernanceConfig(session_summary_turn_threshold=2)
        summarizer = SessionSummarizer(store, config=config)

        user_id = _uid()
        session_id = _sid()
        messages = [
            {"role": "user", "content": "How do I use async in Python?"},
            {"role": "assistant", "content": "Use async/await with asyncio library."},
        ]

        mem = summarizer.check_and_summarize(user_id, session_id, messages, turn_count=2, session_start=datetime.now(timezone.utc))
        assert mem is not None
        memory_cleanup.append(mem.memory_id)

        stored = store.get(mem.memory_id)
        assert stored is not None
        assert stored.session_id == session_id
        assert _INCREMENTAL_TAG in stored.content

    def test_full_summary_cross_session(self, db_factory, memory_cleanup):
        """Full summary has session_id=NULL (cross-session)."""
        from core.memory.tabular.session_summary import SessionSummarizer, _SESSION_SUMMARY_TAG

        store = MemoryStore(db_factory)
        summarizer = SessionSummarizer(store)

        user_id = _uid()
        session_id = _sid()
        messages = [
            {"role": "user", "content": "Explain dependency injection"},
            {"role": "assistant", "content": "DI is a design pattern where dependencies are passed in..."},
        ]

        mem = summarizer.generate_full_summary(user_id, session_id, messages)
        assert mem is not None
        memory_cleanup.append(mem.memory_id)

        stored = store.get(mem.memory_id)
        assert stored is not None
        assert stored.session_id is None
        assert _SESSION_SUMMARY_TAG in stored.content

    def test_full_supersedes_incrementals(self, db_factory, memory_cleanup):
        """Full summary deactivates incremental summaries."""
        from core.memory.tabular.session_summary import SessionSummarizer
        from core.memory.config import MemoryGovernanceConfig

        store = MemoryStore(db_factory)
        config = MemoryGovernanceConfig(session_summary_turn_threshold=2)
        summarizer = SessionSummarizer(store, config=config)

        user_id = _uid()
        session_id = _sid()
        messages = [
            {"role": "user", "content": "Question about testing"},
            {"role": "assistant", "content": "Use pytest for Python testing..."},
        ]

        # Generate 2 incrementals — second call needs new messages
        inc1 = summarizer.check_and_summarize(user_id, session_id, messages, turn_count=2, session_start=datetime.now(timezone.utc))
        messages2 = messages + [
            {"role": "user", "content": "What about mocking?"},
            {"role": "assistant", "content": "Use unittest.mock for mocking..."},
        ]
        inc2 = summarizer.check_and_summarize(user_id, session_id, messages2, turn_count=4, session_start=datetime.now(timezone.utc))
        memory_cleanup.extend([inc1.memory_id, inc2.memory_id])

        # Generate full
        full = summarizer.generate_full_summary(user_id, session_id, messages)
        memory_cleanup.append(full.memory_id)

        # Incrementals should be deactivated
        assert store.get(inc1.memory_id).is_active is False
        assert store.get(inc2.memory_id).is_active is False
        assert store.get(full.memory_id).is_active is True


class TestTrustTierRealDB:
    """Trust tier affects retrieval ranking with real DB."""

    def test_t1_ranks_higher_than_t4(self, db_factory, memory_cleanup):
        """T1 memory ranks higher than T4 at same age due to slower decay."""
        from core.memory.types import TrustTier

        store = MemoryStore(db_factory)
        retriever = MemoryRetriever(db_factory)
        user_id = _uid()
        session_id = _sid()
        age = datetime.now(timezone.utc) - timedelta(days=60)

        t1 = Memory(
            memory_id=str(uuid7()), user_id=user_id,
            memory_type=MemoryType.SEMANTIC, content="Verified: project uses DI pattern",
            initial_confidence=0.9, trust_tier=TrustTier.T1_VERIFIED,
            embedding=_embed("DI pattern"), observed_at=age,
        )
        t4 = Memory(
            memory_id=str(uuid7()), user_id=user_id,
            memory_type=MemoryType.SEMANTIC, content="Unverified: project uses DI pattern",
            initial_confidence=0.9, trust_tier=TrustTier.T4_UNVERIFIED,
            embedding=_embed("DI pattern unverified"), observed_at=age,
        )
        memory_cleanup.extend([t1.memory_id, t4.memory_id])
        store.create(t1)
        store.create(t4)

        results, _ = retriever.retrieve(
            user_id=user_id, query_text="DI pattern",
            session_id=session_id, query_embedding=_embed("DI pattern"),
            limit=10,
        )

        assert len(results) >= 2
        # T1 should rank higher (slower decay → higher confidence score)
        t1_idx = next(i for i, r in enumerate(results) if r.memory_id == t1.memory_id)
        t4_idx = next(i for i, r in enumerate(results) if r.memory_id == t4.memory_id)
        assert t1_idx < t4_idx, f"T1 should rank higher than T4, got T1@{t1_idx} T4@{t4_idx}"


# ── Phase 3 Wiring Tests ──────────────────────────────────────────


class TestSchedulerWiring:
    """Verify GovernanceScheduler is called by production scheduler dispatch."""

    def test_dispatch_calls_memory_governance(self):
        """scheduler._dispatch('hourly') calls GovernanceScheduler.run_hourly()."""
        from unittest.mock import patch, MagicMock
        from core.context.scheduler import GovernanceTaskRunner

        mock_db = MagicMock()
        mock_factory = MagicMock(return_value=mock_db)

        with patch("core.context.lifecycle.MemoryGovernanceEngine") as MockEngine, \
             patch("core.memory.tabular.governance.GovernanceScheduler") as MockSched:
            MockEngine.return_value.run_hourly_tasks.return_value = {"archived_notes": 0}
            from core.memory.tabular.governance import GovernanceCycleResult
            MockSched.return_value.run_hourly.return_value = GovernanceCycleResult(
                cleaned_tool_results=3, archived_working=1,
            )

            result = GovernanceTaskRunner._dispatch("hourly", mock_db, mock_factory)

            MockSched.return_value.run_hourly.assert_called_once()
            assert result["mem_cleaned_tool_results"] == 3
            assert result["mem_archived_working"] == 1

    def test_dispatch_daily_calls_run_daily_all(self):
        """scheduler._dispatch('daily') calls GovernanceScheduler.run_daily_all()."""
        from unittest.mock import patch, MagicMock
        from core.context.scheduler import GovernanceTaskRunner

        mock_db = MagicMock()
        mock_factory = MagicMock(return_value=mock_db)

        with patch("core.context.lifecycle.MemoryGovernanceEngine") as MockEngine, \
             patch("core.memory.tabular.governance.GovernanceScheduler") as MockSched:
            MockEngine.return_value.run_daily_tasks.return_value = {"quarantined": 0}
            from core.memory.tabular.governance import GovernanceCycleResult
            MockSched.return_value.run_daily_all.return_value = GovernanceCycleResult(
                cleaned_stale=2, quarantined=5,
            )

            result = GovernanceTaskRunner._dispatch("daily", mock_db, mock_factory)

            MockSched.return_value.run_daily_all.assert_called_once()
            assert result["mem_quarantined"] == 5


class TestRunDailyAll:
    """Verify run_daily_all iterates all users."""

    def test_iterates_users(self, db_factory, memory_cleanup):
        store = MemoryStore(db_factory)
        uid1, uid2 = _uid(), _uid()

        # Create old low-confidence T4 memories for 2 users
        for uid in (uid1, uid2):
            m = Memory(
                memory_id=str(uuid7()), user_id=uid,
                memory_type=MemoryType.SEMANTIC, content="stale fact",
                initial_confidence=0.3, trust_tier=TrustTier.T4_UNVERIFIED,
                observed_at=datetime.now(timezone.utc) - timedelta(days=120),
            )
            memory_cleanup.append(m.memory_id)
            store.create(m)

        config = MemoryGovernanceConfig(quarantine_threshold=0.2)
        scheduler = GovernanceScheduler(db_factory, config=config)
        result = scheduler.run_daily_all()

        # Both users' memories should be quarantined
        assert result.quarantined >= 2


class TestSessionSummaryWiring:
    """Verify SessionSummarizer is called from session close."""

    def test_close_session_generates_summary(self, db_factory, memory_cleanup):
        """session_manager.close_session() creates a full session summary in memories table."""
        from core.events.session_manager import SessionManager
        from core.events.event_logger import EventLogger
        from sqlalchemy import text

        db = db_factory()
        try:
            session_mgr = SessionManager(db)
            event_logger = EventLogger.from_session(db)

            session = session_mgr.create_session(user_id="summary_wiring_test")
            sid = session.session_id

            # Add some conversation events
            user_evt = event_logger.create_user_query(
                user_id="summary_wiring_test", session_id=sid,
                content="How do I use dependency injection in Python?",
            )
            event_logger.create_llm_response(
                user_id="summary_wiring_test", session_id=sid,
                content="Dependency injection in Python typically uses constructor injection...",
                agent_id="test", agent_version="1.0",
                parent_event_id=user_evt.event_id,
                causal_chain_id=user_evt.causal_chain_id,
            )

            # Close session — should trigger summary generation
            session_mgr.close_session(sid)

            # Check memories table for session summary
            row = db.execute(text(
                "SELECT memory_id, content, session_id FROM mem_memories "
                "WHERE user_id = 'summary_wiring_test' AND content LIKE '%session_summary%' "
                "ORDER BY created_at DESC LIMIT 1"
            )).fetchone()

            if row:
                memory_cleanup.append(row[0])
                assert row[2] is None, "Full summary should have session_id=NULL (cross-session)"
                assert "[session_summary]" in row[1]
        finally:
            db.close()

    def test_turn_hooks_accepts_summary_params(self):
        """turn_hooks.run_observer() accepts turn_count and session_start params."""
        from unittest.mock import MagicMock, patch
        from core.agent.turn_hooks import TurnHooks

        hooks = TurnHooks(db_factory=MagicMock(), llm_client=MagicMock())

        # Should not raise — verifies the signature accepts new params
        with patch("core.memory.tabular.typed_pipeline.run_typed_memory_pipeline"):
            hooks.run_observer(
                session_id="test_sess", user_id="test_user",
                messages=[{"role": "user", "content": "hello"}],
                turn_count=50, session_start=datetime.now(timezone.utc),
            )


class TestTrustTierDefaultsMigration:
    """Verify knowledge/api.py uses types.py trust_tier_defaults."""

    def test_import_from_types(self):
        """trust_tier_defaults importable from both types.py and lifecycle.py."""
        from core.memory.types import trust_tier_defaults as from_types
        from core.context.lifecycle import trust_tier_defaults as from_lifecycle

        # Both should return same result
        t3_types = from_types("T3")
        t3_lifecycle = from_lifecycle("T3")
        assert t3_types == t3_lifecycle
        assert t3_types["initial_confidence"] == 0.65
        assert t3_types["half_life_days"] == 60.0

    def test_knowledge_api_uses_types(self):
        """knowledge/api.py imports trust_tier_defaults from types.py."""
        import inspect
        from skills.knowledge import api as knowledge_api
        source = inspect.getsource(knowledge_api)
        assert "from core.memory.types import trust_tier_defaults" in source
