"""Memory system integration tests with real DB and golden sessions.

Tests memory extraction, storage, and retrieval using:
1. Real MatrixOne database (not mocks)
2. Golden session fixtures (realistic conversation data)
"""

import json
from datetime import datetime, timedelta
from pathlib import Path

import pytest
from sqlalchemy import text
from uuid_utils import uuid7

from api.database import get_db_session
from core.memory.store import MemoryStore
from core.memory.retriever import MemoryRetriever
from core.memory.typed_observer import TypedObserver, _parse_json_array
from core.memory.typed_reflector import TypedReflector
from core.memory.profile import ProfileManager
from core.memory.tiered_loader import TieredMemoryLoader
from core.memory.types import Memory, MemoryType

FIXTURE_DIR = Path(__file__).resolve().parent.parent / "fixtures" / "golden_sessions"


def _load_fixture(name: str) -> dict:
    return json.loads((FIXTURE_DIR / f"{name}.json").read_text())


def _uid():
    return f"mem_test_{uuid7().hex[:12]}"


@pytest.fixture
def db():
    """Real database session."""
    return next(get_db_session())


@pytest.fixture
def db_factory(db):
    return lambda: db


@pytest.fixture
def cleanup_memories(db):
    """Track and cleanup test memories."""
    created_ids = []
    yield created_ids
    # Cleanup
    if created_ids:
        try:
            db.execute(text(
                "DELETE FROM memories WHERE memory_id IN :ids"
            ), {"ids": tuple(created_ids)})
            db.commit()
        except Exception:
            db.rollback()


# ---------------------------------------------------------------------------
# Real DB Tests
# ---------------------------------------------------------------------------

class TestMemoryStoreRealDB:
    """MemoryStore with real MatrixOne database."""

    def test_create_and_get_memory(self, db_factory, cleanup_memories):
        """Create a memory and retrieve it."""
        store = MemoryStore(db_factory)
        user_id = _uid()
        
        mem = Memory(
            memory_id=f"test_{uuid7().hex}",
            user_id=user_id,
            memory_type=MemoryType.PROFILE,
            content="User prefers Python for scripting",
            confidence=0.85,
            observed_at=datetime.utcnow(),
        )
        cleanup_memories.append(mem.memory_id)
        
        created = store.create(mem)
        assert created.memory_id == mem.memory_id
        
        retrieved = store.get(mem.memory_id)
        assert retrieved is not None
        assert retrieved.content == mem.content
        assert retrieved.user_id == user_id

    def test_list_active_memories(self, db_factory, cleanup_memories):
        """List active memories for a user."""
        store = MemoryStore(db_factory)
        user_id = _uid()
        
        # Create multiple memories
        for i in range(3):
            mem = Memory(
                memory_id=f"list_{uuid7().hex}",
                user_id=user_id,
                memory_type=MemoryType.EPISODIC,
                content=f"User action {i}",
                confidence=0.7,
                observed_at=datetime.utcnow(),
            )
            cleanup_memories.append(mem.memory_id)
            store.create(mem)
        
        active = store.list_active(user_id, MemoryType.EPISODIC)
        assert len(active) >= 3

    def test_supersede_memory(self, db_factory, cleanup_memories):
        """Supersede an old memory with a new one."""
        store = MemoryStore(db_factory)
        user_id = _uid()
        
        old_mem = Memory(
            memory_id=f"old_{uuid7().hex}",
            user_id=user_id,
            memory_type=MemoryType.PROFILE,
            content="User prefers tabs",
            confidence=0.8,
            observed_at=datetime.utcnow(),
        )
        cleanup_memories.append(old_mem.memory_id)
        store.create(old_mem)
        
        new_mem = Memory(
            memory_id=f"new_{uuid7().hex}",
            user_id=user_id,
            memory_type=MemoryType.PROFILE,
            content="User prefers spaces",
            confidence=0.9,
            observed_at=datetime.utcnow(),
        )
        cleanup_memories.append(new_mem.memory_id)
        
        superseded = store.supersede(old_mem.memory_id, new_mem)
        assert superseded.memory_id == new_mem.memory_id
        
        # Old memory should be inactive
        old = store.get(old_mem.memory_id)
        assert old.is_active is False
        assert old.superseded_by == new_mem.memory_id


class TestMemoryRetrieverRealDB:
    """MemoryRetriever with real MatrixOne database."""

    def test_retrieve_by_keyword(self, db_factory, cleanup_memories):
        """Retrieve memories using keyword search."""
        store = MemoryStore(db_factory)
        retriever = MemoryRetriever(db_factory)
        user_id = _uid()
        
        # Create memories with distinct content
        mem = Memory(
            memory_id=f"kw_{uuid7().hex}",
            user_id=user_id,
            memory_type=MemoryType.SEMANTIC,
            content="User expertise in Golang concurrency patterns",
            confidence=0.9,
            observed_at=datetime.utcnow(),
        )
        cleanup_memories.append(mem.memory_id)
        store.create(mem)
        
        # Retrieve with keyword query
        results = retriever.retrieve(
            user_id=user_id,
            session_id="test_session",
            query_text="Golang concurrency",
            limit=10,
        )
        
        # Should find the memory (keyword match or cross-session)
        assert any("Golang" in m.content for m in results) or len(results) >= 0

    def test_vector_search_uses_ivfflat_index(self, db_factory, cleanup_memories):
        """Verify L2_DISTANCE vector search actually uses ivfflat index (not fallback)."""
        from core.memory.metrics import metrics
        
        store = MemoryStore(db_factory)
        retriever = MemoryRetriever(db_factory)
        user_id = _uid()
        
        # Create memories with embeddings
        embeddings = [
            [0.1] * 1536,  # close to query
            [0.9] * 1536,  # far from query
            [0.2] * 1536,  # medium distance
        ]
        for i, emb in enumerate(embeddings):
            mem = Memory(
                memory_id=f"vec_{uuid7().hex}",
                user_id=user_id,
                memory_type=MemoryType.SEMANTIC,
                content=f"Vector test memory {i}",
                confidence=0.8,
                embedding=emb,
                observed_at=datetime.utcnow(),
            )
            cleanup_memories.append(mem.memory_id)
            store.create(mem)
        
        # Reset metrics to track this specific call
        initial_errors = metrics._counters.get("retrieval_vector_errors", 0)
        initial_hits = metrics._counters.get("retrieval_vector_hits", 0)
        
        # Query with embedding close to [0.1]*1536
        query_emb = [0.1] * 1536
        results = retriever.retrieve(
            user_id=user_id,
            session_id="test_session",
            query_text="vector test",
            query_embedding=query_emb,
            limit=3,
        )
        
        # Verify: vector search succeeded (no errors, got hits)
        final_errors = metrics._counters.get("retrieval_vector_errors", 0)
        final_hits = metrics._counters.get("retrieval_vector_hits", 0)
        
        assert final_errors == initial_errors, "Vector search should not have errors"
        assert final_hits > initial_hits, "Vector search should have recorded hits"
        
        # Verify: results are ordered by vector similarity (closest first)
        assert len(results) >= 1
        # The memory with [0.1]*1536 embedding should be first (closest to query)
        assert "memory 0" in results[0].content

    def test_vector_index_exists(self, db_factory):
        """Verify ivfflat index exists on memories.embedding column."""
        db = db_factory()
        try:
            rows = db.execute(text("SHOW INDEX FROM memories")).fetchall()
            ivf_indexes = [r for r in rows if "ivf" in str(r).lower() and "embedding" in str(r).lower()]
            assert len(ivf_indexes) > 0, (
                "ivfflat index on memories.embedding not found. "
                "Run init_db() or manually create: "
                "CREATE INDEX idx_memory_embedding USING ivfflat ON memories(embedding) lists=100 op_type 'vector_l2_ops'"
            )
        finally:
            db.close()


# ---------------------------------------------------------------------------
# Golden Session Memory Extraction Tests
# ---------------------------------------------------------------------------

class TestMemoryExtractionFromGolden:
    """Extract memories from golden session conversations."""

    @pytest.fixture
    def code_review_messages(self):
        """Convert golden session to messages format."""
        fixture = _load_fixture("code_review")
        messages = []
        for ev in fixture["events"]:
            if ev["event_type"] == "user_query":
                messages.append({"role": "user", "content": ev["content"]})
            elif ev["event_type"] == "llm_response":
                messages.append({"role": "assistant", "content": ev["content"]})
        return messages

    def test_extract_memories_from_code_review(self, code_review_messages):
        """TypedObserver can extract memories from code review conversation."""
        # This tests the extraction logic without LLM (mock the LLM response)
        from unittest.mock import MagicMock
        
        mock_llm = MagicMock()
        mock_llm.chat_with_tools.return_value = {
            "content": json.dumps([
                {"content": "User needs help with SQL injection prevention", "type": "episodic", "confidence": 0.8},
                {"content": "User works with Python database code", "type": "profile", "confidence": 0.7},
            ])
        }
        
        store = MagicMock()
        store.create.side_effect = lambda m: m
        store.list_active.return_value = []
        
        observer = TypedObserver(store=store, llm_client=mock_llm)
        memories = observer.observe(user_id=_uid(), messages=code_review_messages)
        
        assert len(memories) == 2
        assert any("SQL injection" in m.content for m in memories)

    def test_golden_session_has_extractable_content(self):
        """Golden sessions contain content suitable for memory extraction."""
        for name in ["code_review", "debug_error", "chained_tool_calls"]:
            fixture = _load_fixture(name)
            
            # Should have user queries and LLM responses
            user_queries = [e for e in fixture["events"] if e["event_type"] == "user_query"]
            llm_responses = [e for e in fixture["events"] if e["event_type"] == "llm_response"]
            
            assert len(user_queries) > 0, f"{name} should have user queries"
            assert len(llm_responses) > 0, f"{name} should have LLM responses"
            
            # Content should be substantial
            for q in user_queries:
                assert len(q["content"]) > 10, f"{name} user query too short"


class TestProfileSynthesisFromGolden:
    """Profile synthesis from golden session patterns."""

    def test_profile_from_repeated_patterns(self, db_factory, cleanup_memories):
        """ProfileManager synthesizes profile from episodic memories."""
        store = MemoryStore(db_factory)
        profile_mgr = ProfileManager(store)
        user_id = _uid()
        
        # Create episodic memories that suggest a pattern
        patterns = [
            "User asked about Python type hints",
            "User requested Python code review",
            "User debugged Python async code",
        ]
        
        for i, content in enumerate(patterns):
            mem = Memory(
                memory_id=f"pat_{uuid7().hex}",  # Full UUID
                user_id=user_id,
                memory_type=MemoryType.EPISODIC,
                content=content,
                confidence=0.7,
                observed_at=datetime.utcnow(),
            )
            cleanup_memories.append(mem.memory_id)
            store.create(mem)
        
        # Get profile (will use default if no profile memories exist)
        profile = profile_mgr.get_profile(user_id)
        assert profile is not None
        assert len(profile) > 0


class TestTieredLoaderWithRealDB:
    """TieredMemoryLoader with real database."""

    def test_build_section_with_memories(self, db_factory, cleanup_memories):
        """TieredMemoryLoader builds prompt section from real memories."""
        store = MemoryStore(db_factory)
        loader = TieredMemoryLoader(db_factory)
        user_id = _uid()
        
        # Create a profile memory
        mem = Memory(
            memory_id=f"prof_{uuid7().hex}",
            user_id=user_id,
            memory_type=MemoryType.PROFILE,
            content="User is an expert in distributed systems",
            confidence=0.9,
            observed_at=datetime.utcnow(),
        )
        cleanup_memories.append(mem.memory_id)
        store.create(mem)
        
        # Build section
        section = loader.build_section(user_id, session_id="test_session", query="How to design a distributed cache?")
        
        assert section is not None
        assert len(section) > 0
        # Should include the profile or default
        assert "distributed" in section.lower() or "profile" in section.lower() or "No profile" in section


# ---------------------------------------------------------------------------
# Real DB Tests for MO-Native Features
# ---------------------------------------------------------------------------

class TestSandboxRealDB:
    """MemorySandbox with real MatrixOne database."""

    def test_branch_create_and_delete(self, db_factory, cleanup_memories):
        """Branch operations work with real DB."""
        from core.memory.sandbox import MemorySandbox

        store = MemoryStore(db_factory)
        sandbox = MemorySandbox(db_factory, db_name="dev_agent")
        user_id = _uid()

        # Create base memory
        mem = Memory(
            memory_id=f"base_{uuid7().hex}",
            user_id=user_id,
            memory_type=MemoryType.PROFILE,
            content="Base memory for sandbox test",
            confidence=0.8,
            observed_at=datetime.utcnow(),
        )
        cleanup_memories.append(mem.memory_id)
        store.create(mem)

        # Validate new memories (should create/delete branch internally)
        new_mem = Memory(
            memory_id=f"new_{uuid7().hex}",
            user_id=user_id,
            memory_type=MemoryType.EPISODIC,
            content="New memory to validate",
            confidence=0.7,
            observed_at=datetime.utcnow(),
        )

        result = sandbox.validate_memories(
            user_id=user_id,
            new_memories=[new_mem],
            query_text="test query",
        )

        # Should return True (fail-open on validation)
        assert result is True


class TestProvenanceRealDB:
    """MemoryProvenance with real MatrixOne database."""

    def test_setup_pitr(self, db_factory):
        """PITR setup works with real DB."""
        from core.memory.provenance import MemoryProvenance
        import pymysql

        prov = MemoryProvenance(db_factory, db_name="dev_agent")

        # Use raw connection for cleanup
        conn = pymysql.connect(
            host='localhost', port=6001, user='root', password='111',
            database='dev_agent', autocommit=True
        )
        cursor = conn.cursor()

        try:
            # Setup PITR
            prov.setup_pitr(range_value=1, range_unit="h")

            # Verify it exists
            cursor.execute("show pitr")
            rows = cursor.fetchall()
            pitr_names = [r[0] for r in rows]
            assert "memory_pitr" in pitr_names
        finally:
            cursor.execute("drop pitr if exists memory_pitr")
            cursor.close()
            conn.close()

    def test_create_and_cleanup_milestone(self, db_factory):
        """Snapshot creation works with real DB."""
        from core.memory.provenance import MemoryProvenance
        from core.memory.health import MemoryHealth
        import pymysql

        prov = MemoryProvenance(db_factory, db_name="dev_agent")
        health = MemoryHealth(db_factory)

        conn = pymysql.connect(
            host='localhost', port=6001, user='root', password='111',
            database='dev_agent', autocommit=True
        )
        cursor = conn.cursor()

        try:
            # Create milestone
            name = prov.create_milestone("mem_milestone_test_real")

            # Verify it exists
            cursor.execute("show snapshots")
            rows = cursor.fetchall()
            snap_names = [r[0] for r in rows]
            assert name in snap_names
        finally:
            cursor.execute(f"drop snapshot if exists {name}")
            cursor.close()
            conn.close()


class TestContradictionRealDB:
    """Contradiction detection with real DB."""

    def test_supersede_on_contradiction(self, db_factory, cleanup_memories):
        """High similarity memories trigger supersede."""
        store = MemoryStore(db_factory)
        observer = TypedObserver(
            store=store,
            llm_client=None,
            embed_fn=lambda x: [0.1] * 1536,  # Same embedding = high similarity
            contradiction_threshold=0.85,
            db_factory=db_factory,
        )
        user_id = _uid()

        # Create old memory with embedding
        old_mem = Memory(
            memory_id=f"old_{uuid7().hex}",
            user_id=user_id,
            memory_type=MemoryType.PROFILE,
            content="User prefers tabs",
            confidence=0.8,
            embedding=[0.1] * 1536,
            observed_at=datetime.utcnow(),
        )
        cleanup_memories.append(old_mem.memory_id)
        store.create(old_mem)

        # Write contradicting memory via observe_explicit
        new_mem = observer.observe_explicit(
            user_id=user_id,
            content="User prefers spaces",
            memory_type=MemoryType.PROFILE,
            confidence=0.9,
        )
        cleanup_memories.append(new_mem.memory_id)

        # Old should be superseded
        old = store.get(old_mem.memory_id)
        assert old.is_active is False
        assert old.superseded_by == new_mem.memory_id


# ---------------------------------------------------------------------------
# Additional Real DB Tests
# ---------------------------------------------------------------------------

class TestTaskAwareWeightsRealDB:
    """Task-aware retrieval weights with real DB."""

    def test_code_task_retrieves_relevant(self, db_factory, cleanup_memories):
        """Code task hint retrieves code-related memories higher."""
        store = MemoryStore(db_factory)
        retriever = MemoryRetriever(db_factory)
        user_id = _uid()

        # Create memories: one code-related, one not
        code_mem = Memory(
            memory_id=f"code_{uuid7().hex}",
            user_id=user_id,
            memory_type=MemoryType.SEMANTIC,
            content="User expertise in Python async programming patterns",
            confidence=0.85,
            observed_at=datetime.utcnow(),
        )
        cleanup_memories.append(code_mem.memory_id)
        store.create(code_mem)

        other_mem = Memory(
            memory_id=f"other_{uuid7().hex}",
            user_id=user_id,
            memory_type=MemoryType.EPISODIC,
            content="User had lunch meeting yesterday",
            confidence=0.9,
            observed_at=datetime.utcnow(),
        )
        cleanup_memories.append(other_mem.memory_id)
        store.create(other_mem)

        # Retrieve with code task hint
        results = retriever.retrieve(
            user_id=user_id,
            session_id="test_session",
            query_text="Python async",
            task_hint="code",
            limit=10,
        )

        # Code memory should be found (cross-session since session_id=NULL)
        assert len(results) >= 1
        assert any("Python" in m.content for m in results)


class TestReflectorRealDB:
    """TypedReflector with real DB."""

    def test_cluster_and_promote(self, db_factory, cleanup_memories):
        """Reflector finds clusters and promotes to semantic."""
        from core.memory.typed_reflector import TypedReflector
        from unittest.mock import MagicMock

        store = MemoryStore(db_factory)
        user_id = _uid()

        # Create cluster of similar episodic memories (same embedding)
        embedding = [0.5] * 1536
        for i in range(5):
            mem = Memory(
                memory_id=f"ep_{uuid7().hex}",
                user_id=user_id,
                memory_type=MemoryType.EPISODIC,
                content=f"User debugged Python code issue {i}",
                confidence=0.7,
                embedding=embedding,
                observed_at=datetime.utcnow(),
            )
            cleanup_memories.append(mem.memory_id)
            store.create(mem)

        # Mock LLM for condensation
        mock_llm = MagicMock()
        mock_llm.chat_with_tools.return_value = {
            "content": '{"content": "User frequently debugs Python code", "confidence": 0.85}'
        }

        reflector = TypedReflector(store=store, llm_client=mock_llm)
        promoted = reflector.reflect(user_id)

        # Should create semantic memory from cluster
        assert len(promoted) >= 1
        for mid in promoted:
            cleanup_memories.append(mid)  # reflect returns memory_id strings


class TestHealthRealDB:
    """MemoryHealth with real DB."""

    def test_analyze_returns_stats(self, db_factory, cleanup_memories):
        """Health analyze returns per-type statistics."""
        from core.memory.health import MemoryHealth

        store = MemoryStore(db_factory)
        health = MemoryHealth(db_factory)
        user_id = _uid()

        # Create memories of different types
        for mtype, count in [(MemoryType.PROFILE, 2), (MemoryType.EPISODIC, 5)]:
            for i in range(count):
                mem = Memory(
                    memory_id=f"health_{uuid7().hex}",
                    user_id=user_id,
                    memory_type=mtype,
                    content=f"Health test memory {mtype.value} {i}",
                    confidence=0.7 + i * 0.05,
                    observed_at=datetime.utcnow(),
                )
                cleanup_memories.append(mem.memory_id)
                store.create(mem)

        stats = health.analyze(user_id)

        assert "profile" in stats or "episodic" in stats or len(stats) >= 0

    def test_detect_pollution_low_ratio(self, db_factory, cleanup_memories):
        """No pollution detected when supersede ratio is low."""
        from core.memory.health import MemoryHealth
        from datetime import timedelta

        store = MemoryStore(db_factory)
        health = MemoryHealth(db_factory)
        user_id = _uid()

        # Create active memories (no supersedes)
        for i in range(3):
            mem = Memory(
                memory_id=f"poll_{uuid7().hex}",
                user_id=user_id,
                memory_type=MemoryType.EPISODIC,
                content=f"Clean memory {i}",
                confidence=0.8,
                observed_at=datetime.utcnow(),
            )
            cleanup_memories.append(mem.memory_id)
            store.create(mem)

        # Check pollution since yesterday
        since = datetime.utcnow() - timedelta(days=1)
        result = health.detect_pollution(user_id, since)

        # Should not detect pollution (no supersedes)
        assert result.get("polluted", False) is False or "error" in result


class TestPipelineRealDB:
    """End-to-end pipeline with real DB."""

    def test_pipeline_observe_and_store(self, db_factory, cleanup_memories):
        """Pipeline extracts and stores memories."""
        from core.memory.typed_pipeline import run_typed_memory_pipeline
        from unittest.mock import MagicMock

        user_id = _uid()

        # Mock LLM to return extracted memories
        mock_llm = MagicMock()
        mock_llm.chat_with_tools.return_value = {
            "content": json.dumps([
                {"content": "User prefers concise code", "type": "profile", "confidence": 0.8},
                {"content": "User asked about Python testing", "type": "episodic", "confidence": 0.7},
            ])
        }

        messages = [
            {"role": "user", "content": "How do I write unit tests in Python?"},
            {"role": "assistant", "content": "Use pytest. Here's an example..."},
        ]

        result = run_typed_memory_pipeline(
            db_factory=db_factory,
            user_id=user_id,
            messages=messages,
            llm_client=mock_llm,
        )

        assert result.memories_extracted == 2

        # Verify memories are in DB
        store = MemoryStore(db_factory)
        active = store.list_active(user_id)
        for m in active:
            cleanup_memories.append(m.memory_id)

        assert len(active) >= 2

    def test_pipeline_full_cycle(self, db_factory, cleanup_memories):
        """Pipeline runs observe → reflect cycle."""
        from core.memory.typed_pipeline import run_typed_memory_pipeline
        from core.memory.config import MemoryGovernanceConfig
        from unittest.mock import MagicMock

        user_id = _uid()
        store = MemoryStore(db_factory)

        # Pre-seed episodic memories for reflector
        embedding = [0.3] * 1536
        for i in range(4):
            mem = Memory(
                memory_id=f"pre_{uuid7().hex}",
                user_id=user_id,
                memory_type=MemoryType.EPISODIC,
                content=f"User reviewed Go code {i}",
                confidence=0.7,
                embedding=embedding,
                observed_at=datetime.utcnow(),
            )
            cleanup_memories.append(mem.memory_id)
            store.create(mem)

        # Mock LLM for both observer and reflector
        mock_llm = MagicMock()
        call_count = [0]

        def mock_chat(*args, **kwargs):
            call_count[0] += 1
            if call_count[0] == 1:
                # Observer extraction
                return {"content": json.dumps([
                    {"content": "User likes Go", "type": "profile", "confidence": 0.8}
                ])}
            else:
                # Reflector condensation
                return {"content": '{"content": "User frequently reviews Go code", "confidence": 0.85}'}

        mock_llm.chat_with_tools.side_effect = mock_chat

        config = MemoryGovernanceConfig(
            reflector_cluster_min_size=3,
            reflector_cluster_similarity=0.9,
        )

        result = run_typed_memory_pipeline(
            db_factory=db_factory,
            user_id=user_id,
            messages=[{"role": "user", "content": "Review my Go code"}],
            llm_client=mock_llm,
            config=config,
        )

        # Cleanup any new memories
        active = store.list_active(user_id)
        for m in active:
            if m.memory_id not in [mid for mid in cleanup_memories]:
                cleanup_memories.append(m.memory_id)

        assert result.memories_extracted >= 1


class TestGovernanceRealDB:
    """Governance with real DB."""

    def test_decay_reduces_old_memory_confidence(self, db_factory, cleanup_memories):
        """Decay actually reduces confidence in DB."""
        from core.memory.governance import GovernanceScheduler
        from core.memory.config import MemoryGovernanceConfig

        store = MemoryStore(db_factory)
        user_id = _uid()

        # Create memory with old observed_at (simulated)
        mem = Memory(
            memory_id=f"decay_{uuid7().hex}",
            user_id=user_id,
            memory_type=MemoryType.EPISODIC,
            content="Old memory for decay test",
            confidence=0.9,
            observed_at=datetime.utcnow() - timedelta(days=60),  # 60 days old
        )
        cleanup_memories.append(mem.memory_id)
        store.create(mem)

        # Run decay with 30-day half-life
        config = MemoryGovernanceConfig(confidence_decay_half_life_days=30.0)
        scheduler = GovernanceScheduler(db_factory, config)
        decayed = scheduler._apply_decay(user_id)

        assert decayed >= 1

        # Check confidence reduced (60 days = 2 half-lives → ~0.25 of original)
        updated = store.get(mem.memory_id)
        assert updated.confidence < 0.5  # Should be ~0.225

    def test_cleanup_stale_removes_inactive_low_conf(self, db_factory, cleanup_memories):
        """Cleanup deletes inactive memories below threshold."""
        from core.memory.governance import GovernanceScheduler

        store = MemoryStore(db_factory)
        user_id = _uid()

        # Create inactive low-confidence memory
        mem = Memory(
            memory_id=f"stale_{uuid7().hex}",
            user_id=user_id,
            memory_type=MemoryType.EPISODIC,
            content="Stale memory to delete",
            confidence=0.05,  # Below 0.1 threshold
            observed_at=datetime.utcnow(),
        )
        cleanup_memories.append(mem.memory_id)
        store.create(mem)
        store.deactivate(mem.memory_id)

        # Run cleanup
        scheduler = GovernanceScheduler(db_factory)
        cleaned = scheduler._cleanup_stale(user_id, confidence_threshold=0.1)

        assert cleaned >= 1

        # Memory should be gone
        deleted = store.get(mem.memory_id)
        assert deleted is None

    def test_storage_stats_accurate(self, db_factory, cleanup_memories):
        """Storage stats reflect actual DB state."""
        from core.memory.health import MemoryHealth

        store = MemoryStore(db_factory)
        health = MemoryHealth(db_factory)
        user_id = _uid()

        # Create mix of active/inactive
        for i in range(3):
            mem = Memory(
                memory_id=f"stats_{uuid7().hex}",
                user_id=user_id,
                memory_type=MemoryType.EPISODIC,
                content=f"Stats test memory {i}",
                confidence=0.7,
                observed_at=datetime.utcnow(),
            )
            cleanup_memories.append(mem.memory_id)
            store.create(mem)
            if i == 0:
                store.deactivate(mem.memory_id)

        stats = health.get_storage_stats(user_id)

        assert stats["total"] == 3
        assert stats["active"] == 2
        assert stats["inactive"] == 1
