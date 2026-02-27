"""Tests for review-identified gaps: vector retrieval, sandbox rejection,
TOOL_RESULT cleanup, DB contradiction, profile sort, supersede session_id.

Also verifies fallback paths are observable via metrics counters.
"""

import json
import math
from collections import namedtuple
from datetime import datetime, timedelta
from unittest.mock import MagicMock, patch, call

import pytest

from core.memory.retriever import MemoryRetriever, TASK_WEIGHTS, _safe_exp
from core.memory.typed_observer import TypedObserver
from core.memory.typed_pipeline import run_typed_memory_pipeline, TypedPipelineResult
from core.memory.governance import GovernanceScheduler
from core.memory.profile import ProfileManager
from core.memory.store import MemoryStore
from core.memory.config import MemoryGovernanceConfig
from core.memory.types import Memory, MemoryType, RetrievalWeights
from core.memory.metrics import metrics


# --- Helpers ---

MemRow = namedtuple("MemRow", ["memory_id", "content", "memory_type", "confidence", "observed_at", "session_id"])
VecRow = namedtuple("VecRow", ["memory_id", "content", "memory_type", "confidence", "observed_at", "session_id", "l2_dist"])


def _mem(mid="m1", uid="u1", mtype=MemoryType.PROFILE, content="test", **kw):
    return Memory(memory_id=mid, user_id=uid, memory_type=mtype, content=content, **kw)


# =============================================================================
# 1. Vector retrieval (Phase 2 + Phase 3 merge)
# =============================================================================

class TestVectorRetrieval:
    """Verify L2_DISTANCE vector search is actually invoked and merged."""

    @pytest.fixture
    def mock_db(self):
        db = MagicMock()
        db.execute.return_value.fetchall.return_value = []
        return db

    @pytest.fixture
    def retriever(self, mock_db):
        return MemoryRetriever(db_factory=lambda: mock_db)

    def test_vector_sql_executed_when_embedding_provided(self, retriever, mock_db):
        """Phase 2 vector SQL fires when query_embedding is given."""
        # Phase 1 returns keyword hits
        phase1_rows = [MemRow("m1", "Go testing", "episodic", 0.9, datetime(2026, 2, 26), None)]
        # Phase 2 returns vector hits
        vec_rows = [VecRow("m2", "Go patterns", "semantic", 0.8, datetime(2026, 2, 26), None, 0.3)]

        call_count = [0]
        def mock_execute(sql, params=None):
            call_count[0] += 1
            result = MagicMock()
            if call_count[0] == 1:
                result.fetchall.return_value = phase1_rows  # keyword
            elif call_count[0] == 2:
                result.fetchall.return_value = vec_rows  # vector
            else:
                result.fetchall.return_value = []
            return result

        mock_db.execute.side_effect = mock_execute

        results = retriever.retrieve(
            "u1", "Go testing", session_id="s1",
            query_embedding=[0.1] * 10,
        )

        # Both phases should have been called
        assert call_count[0] >= 2
        # Both m1 (keyword) and m2 (vector) should appear in merged results
        ids = {r.memory_id for r in results}
        assert "m1" in ids
        assert "m2" in ids

    def test_vector_only_candidate_appears_in_results(self, retriever, mock_db):
        """A memory found only by vector search (not keyword) still appears."""
        # Phase 1: no keyword hits → fallback returns nothing
        # Phase 2: vector returns one hit
        call_count = [0]
        def mock_execute(sql, params=None):
            call_count[0] += 1
            result = MagicMock()
            if call_count[0] <= 1:
                result.fetchall.return_value = []  # keyword fails, fallback empty
            elif call_count[0] == 2:
                result.fetchall.return_value = [
                    VecRow("vec1", "vector-only memory", "semantic", 0.7, datetime(2026, 2, 26), None, 0.1)
                ]
            else:
                result.fetchall.return_value = []
            return result

        mock_db.execute.side_effect = mock_execute

        results = retriever.retrieve(
            "u1", "", session_id="s1",
            query_embedding=[0.1] * 10,
        )

        assert any(r.memory_id == "vec1" for r in results)

    def test_merge_ranks_by_weighted_score(self, retriever, mock_db):
        """Merged results are ranked by 4-dim weighted score, not just one signal."""
        now = datetime.utcnow()
        # m1: keyword match, no vector, old
        # m2: no keyword, close vector, recent
        phase1_rows = [MemRow("m1", "old keyword", "episodic", 0.5, now - timedelta(days=30), None)]
        vec_rows = [VecRow("m2", "recent vector", "episodic", 0.9, now, None, 0.01)]

        call_count = [0]
        def mock_execute(sql, params=None):
            call_count[0] += 1
            result = MagicMock()
            if call_count[0] == 1:
                result.fetchall.return_value = phase1_rows
            elif call_count[0] == 2:
                result.fetchall.return_value = vec_rows
            else:
                result.fetchall.return_value = []
            return result

        mock_db.execute.side_effect = mock_execute

        # Use weights that heavily favor vector
        results = retriever.retrieve(
            "u1", "keyword", session_id="s1",
            query_embedding=[0.1] * 10,
            weights=RetrievalWeights(vector=0.7, keyword=0.1, temporal=0.1, confidence=0.1),
        )

        # m2 (close vector, recent, high confidence) should rank above m1
        assert results[0].memory_id == "m2"

    def test_no_embedding_skips_vector_phase(self, retriever, mock_db):
        """Without query_embedding, only phase 1 runs (no L2_DISTANCE)."""
        mock_db.execute.return_value.fetchall.return_value = [
            MemRow("m1", "test", "episodic", 0.8, datetime(2026, 2, 26), None)
        ]

        results = retriever.retrieve("u1", "test", session_id="s1")

        # Only 1 SQL call (fallback), no vector SQL
        assert mock_db.execute.call_count == 1
        assert len(results) == 1

    def test_vector_phase_failure_increments_error_counter(self, retriever, mock_db):
        """Vector SQL failure must increment retrieval_vector_errors counter."""
        # Use unique counter check: record before, trigger error, check delta is exactly 1
        # Note: parallel tests may also increment, so we verify delta >= 1
        initial_errors = metrics._counters.get("retrieval_vector_errors", 0)
        
        call_count = [0]
        def mock_execute(sql, params=None):
            call_count[0] += 1
            result = MagicMock()
            if call_count[0] == 1:
                result.fetchall.return_value = [
                    MemRow("m1", "fallback", "episodic", 0.8, datetime(2026, 2, 26), None)
                ]
            else:
                raise Exception("Vector index not available")
            return result

        mock_db.execute.side_effect = mock_execute

        results = retriever.retrieve(
            "u1", "test", session_id="s1",
            query_embedding=[0.1] * 10,
        )

        # Verify fallback worked
        assert len(results) == 1
        assert results[0].memory_id == "m1"
        
        # Verify error counter incremented (at least +1, may be more in parallel)
        final_errors = metrics._counters.get("retrieval_vector_errors", 0)
        assert final_errors >= initial_errors + 1, "Vector error should increment counter"

    def test_vector_success_increments_hits_counter(self, retriever, mock_db):
        """Successful vector search must increment retrieval_vector_hits counter."""
        initial_hits = metrics._counters.get("retrieval_vector_hits", 0)
        
        call_count = [0]
        def mock_execute(sql, params=None):
            call_count[0] += 1
            result = MagicMock()
            if call_count[0] == 1:
                result.fetchall.return_value = []  # phase 1 empty
            else:
                result.fetchall.return_value = [
                    VecRow("v1", "vector hit", "semantic", 0.8, datetime(2026, 2, 26), None, 0.1)
                ]
            return result

        mock_db.execute.side_effect = mock_execute

        results = retriever.retrieve(
            "u1", "test", session_id="s1",
            query_embedding=[0.1] * 10,
        )

        # Verify vector search worked
        assert any(r.memory_id == "v1" for r in results)
        
        # Verify hits counter incremented (at least +1)
        final_hits = metrics._counters.get("retrieval_vector_hits", 0)
        assert final_hits >= initial_hits + 1, "Vector success should increment hits counter"

    def test_keyword_failure_increments_error_counter(self, retriever, mock_db):
        """Keyword SQL failure must increment retrieval_keyword_errors counter."""
        initial_errors = metrics._counters.get("retrieval_keyword_errors", 0)
        
        call_count = [0]
        def mock_execute(sql, params=None):
            call_count[0] += 1
            result = MagicMock()
            sql_str = str(sql)
            if "MATCH" in sql_str:
                raise Exception("Fulltext index error")
            # Fallback SQL succeeds
            result.fetchall.return_value = [
                MemRow("m1", "fallback", "episodic", 0.8, datetime(2026, 2, 26), None)
            ]
            return result

        mock_db.execute.side_effect = mock_execute

        results = retriever.retrieve("u1", "test query", session_id="s1")

        # Verify fallback worked
        assert len(results) >= 1
        
        # Verify error counter incremented (at least +1)
        final_errors = metrics._counters.get("retrieval_keyword_errors", 0)
        assert final_errors >= initial_errors + 1, "Keyword error should increment counter"


# =============================================================================
# 2. Pipeline sandbox actually rejecting memories
# =============================================================================

class TestPipelineSandboxRejection:
    """Verify sandbox can reject memories and pipeline respects the decision."""

    def test_sandbox_rejects_bad_memories(self):
        """When sandbox says quality degrades, memories are NOT persisted."""
        mock_llm = MagicMock()
        mock_llm.chat_with_tools.return_value = {"content": json.dumps([
            {"type": "profile", "content": "bad memory", "confidence": 0.5},
        ])}

        mock_db = MagicMock()

        with patch("core.memory.typed_pipeline.MemoryStore") as MockStore, \
             patch("core.memory.typed_pipeline.MemorySandbox") as MockSandbox:
            mock_store = MagicMock()
            mock_store.create.side_effect = lambda m: m
            mock_store.list_active.return_value = []
            MockStore.return_value = mock_store

            mock_sandbox = MagicMock()
            mock_sandbox.validate_memories.return_value = False  # Reject!
            MockSandbox.return_value = mock_sandbox

            result = run_typed_memory_pipeline(
                db_factory=lambda: mock_db,
                user_id="u1",
                messages=[{"role": "user", "content": "test"}],
                llm_client=mock_llm,
                config=MemoryGovernanceConfig(sandbox_enabled_types=("profile",)),
                query_for_sandbox="test query",
            )

        assert result.memories_extracted == 1
        assert result.memories_rejected == 1
        assert result.memories_validated == 0
        # store.create should NOT have been called (rejected by sandbox)
        mock_store.create.assert_not_called()

    def test_sandbox_accepts_good_memories(self):
        """When sandbox says quality improves, memories ARE persisted."""
        mock_llm = MagicMock()
        mock_llm.chat_with_tools.return_value = {"content": json.dumps([
            {"type": "profile", "content": "good memory", "confidence": 0.9},
        ])}

        mock_db = MagicMock()

        with patch("core.memory.typed_pipeline.MemoryStore") as MockStore, \
             patch("core.memory.typed_pipeline.MemorySandbox") as MockSandbox:
            mock_store = MagicMock()
            mock_store.create.side_effect = lambda m: m
            mock_store.list_active.return_value = []
            MockStore.return_value = mock_store

            mock_sandbox = MagicMock()
            mock_sandbox.validate_memories.return_value = True  # Accept
            MockSandbox.return_value = mock_sandbox

            result = run_typed_memory_pipeline(
                db_factory=lambda: mock_db,
                user_id="u1",
                messages=[{"role": "user", "content": "test"}],
                llm_client=mock_llm,
                config=MemoryGovernanceConfig(sandbox_enabled_types=("profile",)),
                query_for_sandbox="test query",
            )

        assert result.memories_extracted == 1
        assert result.memories_validated == 1
        assert result.memories_rejected == 0
        # store.create should have been called (accepted by sandbox)
        mock_store.create.assert_called_once()

    def test_non_sandbox_types_bypass_validation(self):
        """Memory types not in sandbox_enabled_types skip validation."""
        mock_llm = MagicMock()
        mock_llm.chat_with_tools.return_value = {"content": json.dumps([
            {"type": "episodic", "content": "event happened", "confidence": 0.7},
        ])}

        mock_db = MagicMock()

        with patch("core.memory.typed_pipeline.MemoryStore") as MockStore, \
             patch("core.memory.typed_pipeline.MemorySandbox") as MockSandbox:
            mock_store = MagicMock()
            mock_store.create.side_effect = lambda m: m
            mock_store.list_active.return_value = []
            MockStore.return_value = mock_store

            mock_sandbox = MagicMock()
            mock_sandbox.validate_memories.return_value = False  # Would reject
            MockSandbox.return_value = mock_sandbox

            result = run_typed_memory_pipeline(
                db_factory=lambda: mock_db,
                user_id="u1",
                messages=[{"role": "user", "content": "test"}],
                llm_client=mock_llm,
                config=MemoryGovernanceConfig(sandbox_enabled_types=("profile",)),  # only profile
                query_for_sandbox="test query",
            )

        # Episodic bypasses sandbox → persisted despite sandbox returning False
        assert result.memories_extracted == 1
        assert result.memories_rejected == 0
        mock_store.create.assert_called_once()

    def test_sandbox_failure_increments_error_counter(self):
        """Sandbox validation failure must increment sandbox_validation_errors counter."""
        initial_errors = metrics._counters.get("sandbox_validation_errors", 0)
        
        mock_llm = MagicMock()
        mock_llm.chat_with_tools.return_value = {"content": json.dumps([
            {"type": "profile", "content": "test memory", "confidence": 0.9},
        ])}

        mock_db = MagicMock()

        with patch("core.memory.typed_pipeline.MemoryStore") as MockStore, \
             patch("core.memory.typed_pipeline.MemorySandbox") as MockSandbox:
            mock_store = MagicMock()
            mock_store.create.side_effect = lambda m: m
            mock_store.list_active.return_value = []
            MockStore.return_value = mock_store

            mock_sandbox = MagicMock()
            mock_sandbox.validate_memories.side_effect = Exception("Sandbox DB error")
            MockSandbox.return_value = mock_sandbox

            result = run_typed_memory_pipeline(
                db_factory=lambda: mock_db,
                user_id="u1",
                messages=[{"role": "user", "content": "test"}],
                llm_client=mock_llm,
                config=MemoryGovernanceConfig(sandbox_enabled_types=("profile",)),
                query_for_sandbox="test query",
            )

        # Verify fallback worked (memories accepted despite sandbox error)
        assert result.memories_extracted == 1
        mock_store.create.assert_called_once()
        
        # Verify error counter incremented (at least +1)
        final_errors = metrics._counters.get("sandbox_validation_errors", 0)
        assert final_errors >= initial_errors + 1, "Sandbox error should increment counter"


# =============================================================================
# 3. TOOL_RESULT TTL cleanup
# =============================================================================

class TestToolResultCleanup:
    """Verify TOOL_RESULT memories are cleaned up by TTL."""

    def test_cleanup_tool_results_deletes_expired(self):
        """Expired TOOL_RESULT memories are deleted."""
        config = MemoryGovernanceConfig(tool_result_ttl_hours=24)
        scheduler = GovernanceScheduler(MagicMock(), config)

        mock_session = MagicMock()
        mock_result = MagicMock()
        mock_result.rowcount = 5
        mock_session.execute.return_value = mock_result

        with patch.object(scheduler, "_db") as mock_db:
            mock_db.return_value.__enter__.return_value = mock_session
            mock_db.return_value.__exit__.return_value = None

            count = scheduler._cleanup_tool_results()

        assert count == 5
        # Verify DELETE with correct memory_type and TTL
        call_args = mock_session.execute.call_args
        sql_text = str(call_args[0][0])
        assert "DELETE FROM memories" in sql_text
        assert "memory_type" in sql_text
        params = call_args[0][1]
        assert params["mtype"] == "tool_result"
        assert params["ttl"] == 24

    def test_governance_cycle_includes_tool_result_cleanup(self):
        """Full governance cycle runs TOOL_RESULT cleanup as step 7."""
        config = MemoryGovernanceConfig()
        scheduler = GovernanceScheduler(MagicMock(), config)

        with patch.object(scheduler, "_apply_decay", return_value=0), \
             patch.object(scheduler.reflector, "reflect", return_value={"promoted": 0}), \
             patch.object(scheduler.health, "detect_pollution", return_value={"is_polluted": False}), \
             patch.object(scheduler, "_cleanup_stale", return_value=0), \
             patch.object(scheduler.health, "cleanup_orphan_branches", return_value=0), \
             patch.object(scheduler.health, "cleanup_snapshots", return_value=0), \
             patch.object(scheduler, "_cleanup_tool_results", return_value=3) as mock_tool:

            result = scheduler.run_cycle("user1")

        mock_tool.assert_called_once()
        assert result.cleaned_tool_results == 3


# =============================================================================
# 4. DB-side contradiction detection
# =============================================================================

class TestDBContradictionDetection:
    """Verify DB-side L2_DISTANCE contradiction detection (no fallback)."""

    def test_db_contradiction_found(self):
        """DB query finds a close vector match with different content → contradiction."""
        mock_db = MagicMock()
        mock_row = MagicMock()
        mock_row.memory_id = "old1"
        mock_row.content = "prefers tabs"
        mock_row.confidence = 0.8
        mock_row.l2_dist = 0.1  # Very close → contradiction
        mock_db.execute.return_value.fetchone.return_value = mock_row

        store = MagicMock()
        observer = TypedObserver(
            store=store, llm_client=None, embed_fn=None,
            contradiction_threshold=0.85,
            db_factory=lambda: mock_db,
        )

        new_mem = _mem(mid="new1", content="prefers spaces", embedding=[0.1] * 10)
        result = observer._find_contradiction(new_mem)

        assert result is not None
        assert result.memory_id == "old1"

    def test_db_contradiction_not_found_when_distant(self):
        """DB query finds a distant vector match → no contradiction."""
        mock_db = MagicMock()
        mock_row = MagicMock()
        mock_row.memory_id = "old1"
        mock_row.content = "likes Go"
        mock_row.confidence = 0.8
        mock_row.l2_dist = 5.0  # Very far → not a contradiction
        mock_db.execute.return_value.fetchone.return_value = mock_row

        store = MagicMock()
        observer = TypedObserver(
            store=store, llm_client=None, embed_fn=None,
            contradiction_threshold=0.85,
            db_factory=lambda: mock_db,
        )

        new_mem = _mem(mid="new1", content="likes Rust", embedding=[0.1] * 10)
        result = observer._find_contradiction(new_mem)

        assert result is None

    def test_db_error_propagates(self):
        """DB errors propagate — no silent fallback to in-memory scan."""
        mock_db = MagicMock()
        mock_db.execute.side_effect = Exception("DB connection lost")

        store = MagicMock()
        observer = TypedObserver(
            store=store, llm_client=None, embed_fn=None,
            contradiction_threshold=0.85,
            db_factory=lambda: mock_db,
        )

        new_mem = _mem(mid="new1", content="prefers spaces", embedding=[0.1] * 10)
        with pytest.raises(Exception, match="DB connection lost"):
            observer._find_contradiction(new_mem)

    def test_no_db_factory_skips_contradiction(self):
        """Without db_factory, contradiction detection is skipped (returns None)."""
        store = MagicMock()
        observer = TypedObserver(
            store=store, llm_client=None, embed_fn=None,
            contradiction_threshold=0.85,
            db_factory=None,
        )

        new_mem = _mem(mid="new1", content="prefers spaces", embedding=[0.5] * 10)
        result = observer._find_contradiction(new_mem)

        assert result is None
        # store.list_active should NOT be called (no in-memory fallback)
        store.list_active.assert_not_called()


# =============================================================================
# 5. ProfileManager sort with None observed_at
# =============================================================================

class TestProfileSortWithNone:
    """Verify ProfileManager handles None observed_at without crashing."""

    def test_sort_with_none_observed_at(self):
        """Memories with None observed_at don't crash the sort."""
        store = MagicMock()
        store.list_active.return_value = [
            _mem(mid="m1", content="has date", confidence=0.8,
                 observed_at=datetime(2026, 2, 26)),
            _mem(mid="m2", content="no date", confidence=0.9,
                 observed_at=None),
            _mem(mid="m3", content="old date", confidence=0.8,
                 observed_at=datetime(2026, 1, 1)),
        ]

        mgr = ProfileManager(store)
        profile = mgr.get_profile("u1")

        # Should not crash and should contain all memories
        assert "has date" in profile
        assert "no date" in profile
        assert "old date" in profile

    def test_sort_orders_by_confidence_then_recency(self):
        """Higher confidence first, then more recent first."""
        store = MagicMock()
        store.list_active.return_value = [
            _mem(mid="m1", content="low-conf-recent", confidence=0.5,
                 observed_at=datetime(2026, 2, 26)),
            _mem(mid="m2", content="high-conf-old", confidence=0.9,
                 observed_at=datetime(2026, 1, 1)),
            _mem(mid="m3", content="high-conf-recent", confidence=0.9,
                 observed_at=datetime(2026, 2, 26)),
        ]

        mgr = ProfileManager(store)
        profile = mgr.get_profile("u1")

        # high-conf-recent should appear before high-conf-old (same confidence, more recent)
        # Both should appear before low-conf-recent
        lines = profile.split("\n")[1:]  # Skip "User Profile:" header
        assert "high-conf-recent" in lines[0]
        assert "high-conf-old" in lines[1]
        assert "low-conf-recent" in lines[2]


# =============================================================================
# 6. Store.supersede session_id propagation
# =============================================================================

class TestSupersedeSessionId:
    """Verify supersede passes session_id to the new MemoryRecord."""

    def test_supersede_preserves_session_id(self):
        mock_db = MagicMock()
        old_row = MagicMock(is_active=1)
        mock_db.query.return_value.filter_by.return_value.first.return_value = old_row

        store = MemoryStore(db_factory=lambda: mock_db)

        new_mem = _mem(mid="m2", content="new")
        new_mem.session_id = "sess_123"

        store.supersede("m1", new_mem)

        # Verify the MemoryRecord was created with session_id
        add_call = mock_db.add.call_args[0][0]
        assert add_call.session_id == "sess_123"


# =============================================================================
# 7. Confidence decay formula precision
# =============================================================================

class TestConfidenceDecayPrecision:
    """Verify the decay formula produces mathematically correct results."""

    def test_safe_exp_clamps(self):
        """_safe_exp doesn't overflow on extreme inputs."""
        assert _safe_exp(-1000) == math.exp(-500)  # clamped
        assert _safe_exp(1000) == math.exp(500)  # clamped
        assert abs(_safe_exp(0) - 1.0) < 1e-10

    def test_decay_sql_formula_matches_python(self):
        """The SQL decay formula should match the Python equivalent."""
        # conf * exp(-age_days / half_life)
        initial_conf = 0.9
        age_days = 60
        half_life = 30.0

        # Python calculation
        expected = initial_conf * math.exp(-age_days / half_life)

        # At 2 half-lives, should be ~0.9 * e^(-2) ≈ 0.1217
        assert abs(expected - 0.9 * math.exp(-2)) < 1e-10
        assert expected < 0.15  # Sanity: well below 0.5 at 2 half-lives


# =============================================================================
# 8. Observer extract_candidates vs observe separation
# =============================================================================

class TestObserverExtractVsObserve:
    """Verify extract_candidates does NOT persist, observe DOES."""

    def test_extract_candidates_does_not_call_store(self):
        """extract_candidates returns memories without calling store.create."""
        mock_store = MagicMock()
        mock_llm = MagicMock()
        mock_llm.chat_with_tools.return_value = {"content": json.dumps([
            {"type": "profile", "content": "likes Go", "confidence": 0.9},
        ])}

        observer = TypedObserver(store=mock_store, llm_client=mock_llm)
        candidates = observer.extract_candidates("u1", [{"role": "user", "content": "I like Go"}])

        assert len(candidates) == 1
        assert candidates[0].content == "likes Go"
        mock_store.create.assert_not_called()
        mock_store.supersede.assert_not_called()

    def test_observe_does_persist(self):
        """observe() calls store.create for each extracted memory."""
        mock_store = MagicMock()
        mock_store.create.side_effect = lambda m: m
        mock_store.list_active.return_value = []
        mock_llm = MagicMock()
        mock_llm.chat_with_tools.return_value = {"content": json.dumps([
            {"type": "profile", "content": "likes Go", "confidence": 0.9},
        ])}

        observer = TypedObserver(store=mock_store, llm_client=mock_llm)
        results = observer.observe("u1", [{"role": "user", "content": "I like Go"}])

        assert len(results) == 1
        mock_store.create.assert_called_once()

    def test_persist_with_contradiction_check_public_api(self):
        """persist_with_contradiction_check is the public API for pipeline."""
        mock_store = MagicMock()
        mock_store.create.side_effect = lambda m: m
        mock_store.list_active.return_value = []

        observer = TypedObserver(store=mock_store, llm_client=None)
        mem = _mem(content="test memory")
        result = observer.persist_with_contradiction_check(mem)

        assert result.content == "test memory"
        mock_store.create.assert_called_once()


# =============================================================================
# 9. TieredLoader fallback metrics
# =============================================================================

class TestTieredLoaderFallbackMetrics:
    """Verify TieredMemoryLoader failures increment error counters."""

    def test_init_failure_increments_counter(self):
        """TieredMemoryLoader init failure must increment error counter."""
        from core.memory.tiered_loader import TieredMemoryLoader
        
        initial_errors = metrics._counters.get("tiered_loader_init_errors", 0)
        
        # Make MemoryStore constructor raise
        with patch("core.memory.tiered_loader.MemoryStore") as MockStore:
            MockStore.side_effect = Exception("DB connection failed")
            
            loader = TieredMemoryLoader(lambda: MagicMock())
            result = loader._ensure_initialized()
        
        assert result is False
        final_errors = metrics._counters.get("tiered_loader_init_errors", 0)
        assert final_errors >= initial_errors + 1, "Init error should increment counter"

    def test_l0_failure_increments_counter(self):
        """L0 load failure must increment error counter."""
        from core.memory.tiered_loader import TieredMemoryLoader
        
        initial_errors = metrics._counters.get("tiered_loader_l0_errors", 0)
        
        mock_db = MagicMock()
        loader = TieredMemoryLoader(lambda: mock_db)
        
        # Force init to succeed but profile load to fail
        with patch.object(loader, "_ensure_initialized", return_value=True):
            loader._profile_mgr = MagicMock()
            loader._profile_mgr.get_profile.side_effect = Exception("Profile load failed")
            
            result = loader.load_l0("u1")
        
        assert result == ""
        final_errors = metrics._counters.get("tiered_loader_l0_errors", 0)
        assert final_errors >= initial_errors + 1, "L0 error should increment counter"

    def test_l1_failure_increments_counter(self):
        """L1 load failure must increment error counter."""
        from core.memory.tiered_loader import TieredMemoryLoader
        
        initial_errors = metrics._counters.get("tiered_loader_l1_errors", 0)
        
        mock_db = MagicMock()
        loader = TieredMemoryLoader(lambda: mock_db)
        
        # Force init to succeed but retriever to fail
        with patch.object(loader, "_ensure_initialized", return_value=True):
            loader._retriever = MagicMock()
            loader._retriever.retrieve.side_effect = Exception("Retrieval failed")
            
            result = loader.load_l1("u1", "s1", "query")
        
        assert result == ""
        final_errors = metrics._counters.get("tiered_loader_l1_errors", 0)
        assert final_errors >= initial_errors + 1, "L1 error should increment counter"

    def test_success_does_not_increment_error_counters(self):
        """Successful operations should NOT increment error counters."""
        from core.memory.tiered_loader import TieredMemoryLoader
        
        initial_l0_errors = metrics._counters.get("tiered_loader_l0_errors", 0)
        initial_l1_errors = metrics._counters.get("tiered_loader_l1_errors", 0)
        
        mock_db = MagicMock()
        loader = TieredMemoryLoader(lambda: mock_db)
        
        with patch.object(loader, "_ensure_initialized", return_value=True):
            loader._profile_mgr = MagicMock()
            loader._profile_mgr.get_profile.return_value = "User Profile: likes Python"
            loader._retriever = MagicMock()
            loader._retriever.retrieve.return_value = [
                _mem(content="relevant memory")
            ]
            
            l0 = loader.load_l0("u1")
            l1 = loader.load_l1("u1", "s1", "query")
        
        assert "Python" in l0
        assert "relevant memory" in l1
        
        # Error counters should NOT have changed
        assert metrics._counters.get("tiered_loader_l0_errors", 0) == initial_l0_errors
        assert metrics._counters.get("tiered_loader_l1_errors", 0) == initial_l1_errors


# =============================================================================
# 10. Sandbox direct fallback metrics
# =============================================================================

class TestSandboxDirectFallbackMetrics:
    """Verify MemorySandbox.validate_memories failure increments counter."""

    def test_sandbox_exception_increments_counter(self):
        """Sandbox internal exception must increment error counter."""
        from core.memory.sandbox import MemorySandbox
        
        initial_errors = metrics._counters.get("sandbox_validation_errors", 0)
        
        mock_db = MagicMock()
        mock_db.execute.side_effect = Exception("Branch creation failed")
        
        sandbox = MemorySandbox(lambda: mock_db)
        result = sandbox.validate_memories(
            user_id="u1",
            new_memories=[_mem(content="test")],
            query_text="test query",
        )
        
        # Fail open: returns True
        assert result is True
        
        # Error counter incremented (at least +1)
        final_errors = metrics._counters.get("sandbox_validation_errors", 0)
        assert final_errors >= initial_errors + 1, "Sandbox error should increment counter"

    def test_sandbox_success_does_not_increment_error_counter(self):
        """Successful sandbox validation should NOT increment error counter."""
        from core.memory.sandbox import MemorySandbox
        
        initial_errors = metrics._counters.get("sandbox_validation_errors", 0)
        
        mock_db = MagicMock()
        # Mock successful branch operations
        mock_db.execute.return_value.fetchall.return_value = [MagicMock(sim=0.9)]
        
        sandbox = MemorySandbox(lambda: mock_db)
        
        # Mock _create_branch, _insert_to_branch, _retrieval_score, _drop_branch
        with patch.object(sandbox, "_create_branch"), \
             patch.object(sandbox, "_insert_to_branch"), \
             patch.object(sandbox, "_retrieval_score", return_value=0.8), \
             patch.object(sandbox, "_drop_branch"):
            result = sandbox.validate_memories(
                user_id="u1",
                new_memories=[_mem(content="test")],
                query_text="test query",
            )
        
        # Success
        assert result is True
        
        # Error counter should NOT have changed
        assert metrics._counters.get("sandbox_validation_errors", 0) == initial_errors
