"""Tests for review-identified gaps: vector retrieval, sandbox rejection,
TOOL_RESULT cleanup, DB contradiction, profile sort, supersede session_id.

Also verifies fallback paths are observable via explain stats.
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
from core.memory.metrics import MemoryMetrics


# --- Helpers ---

MemRow = namedtuple("MemRow", ["memory_id", "content", "memory_type", "initial_confidence", "observed_at", "session_id"])
VecRow = namedtuple("VecRow", ["memory_id", "content", "memory_type", "initial_confidence", "observed_at", "session_id", "l2_dist"])


def _mem(mid="m1", uid="u1", mtype=MemoryType.PROFILE, content="test", **kw):
    return Memory(memory_id=mid, user_id=uid, memory_type=mtype, content=content, **kw)


# =============================================================================
# 1. Vector retrieval (Phase 2 + Phase 3 merge)
# =============================================================================

class TestVectorRetrieval:

    @pytest.fixture
    def mock_db(self):
        db = MagicMock()
        db.execute.return_value.fetchall.return_value = []
        return db

    @pytest.fixture
    def retriever(self, mock_db):
        return MemoryRetriever(db_factory=lambda: mock_db)

    def test_vector_sql_executed_when_embedding_provided(self, retriever, mock_db):
        phase1_rows = [MemRow("m1", "Go testing", "semantic", 0.9, datetime(2026, 2, 26), None)]
        vec_rows = [VecRow("m2", "Go patterns", "semantic", 0.8, datetime(2026, 2, 26), None, 0.3)]

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
        results, _ = retriever.retrieve("u1", "Go testing", session_id="s1", query_embedding=[0.1] * 10)
        assert call_count[0] >= 2
        ids = {r.memory_id for r in results}
        assert "m1" in ids
        assert "m2" in ids

    def test_vector_only_candidate_appears_in_results(self, retriever, mock_db):
        call_count = [0]
        def mock_execute(sql, params=None):
            call_count[0] += 1
            result = MagicMock()
            if call_count[0] <= 1:
                result.fetchall.return_value = []
            elif call_count[0] == 2:
                result.fetchall.return_value = [
                    VecRow("vec1", "vector-only memory", "semantic", 0.7, datetime(2026, 2, 26), None, 0.1)
                ]
            else:
                result.fetchall.return_value = []
            return result

        mock_db.execute.side_effect = mock_execute
        results, _ = retriever.retrieve("u1", "", session_id="s1", query_embedding=[0.1] * 10)
        assert any(r.memory_id == "vec1" for r in results)

    def test_merge_ranks_by_weighted_score(self, retriever, mock_db):
        now = datetime.utcnow()
        phase1_rows = [MemRow("m1", "old keyword", "semantic", 0.5, now - timedelta(days=30), None)]
        vec_rows = [VecRow("m2", "recent vector", "semantic", 0.9, now, None, 0.01)]

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
        results, _ = retriever.retrieve(
            "u1", "keyword", session_id="s1", query_embedding=[0.1] * 10,
            weights=RetrievalWeights(vector=0.7, keyword=0.1, temporal=0.1, confidence=0.1),
        )
        assert results[0].memory_id == "m2"

    def test_no_embedding_skips_vector_phase(self, retriever, mock_db):
        mock_db.execute.return_value.fetchall.return_value = [
            MemRow("m1", "test", "semantic", 0.8, datetime(2026, 2, 26), None)
        ]
        results, _ = retriever.retrieve("u1", "test", session_id="s1")
        assert mock_db.execute.call_count == 1
        assert len(results) == 1

    def test_vector_phase_failure_recorded_in_stats(self, retriever, mock_db):
        call_count = [0]
        def mock_execute(sql, params=None):
            call_count[0] += 1
            result = MagicMock()
            if call_count[0] == 1:
                result.fetchall.return_value = [
                    MemRow("m1", "fallback", "semantic", 0.8, datetime(2026, 2, 26), None)
                ]
            else:
                raise Exception("Vector index not available")
            return result

        mock_db.execute.side_effect = mock_execute
        results, stats = retriever.retrieve(
            "u1", "test", session_id="s1", query_embedding=[0.1] * 10, explain=True,
        )
        assert len(results) == 1
        assert stats.vector_attempted is True
        assert "Vector index not available" in stats.vector_error

    def test_vector_success_recorded_in_stats(self, retriever, mock_db):
        call_count = [0]
        def mock_execute(sql, params=None):
            call_count[0] += 1
            result = MagicMock()
            if call_count[0] == 1:
                result.fetchall.return_value = []
            else:
                result.fetchall.return_value = [
                    VecRow("v1", "vector hit", "semantic", 0.8, datetime(2026, 2, 26), None, 0.1)
                ]
            return result

        mock_db.execute.side_effect = mock_execute
        results, stats = retriever.retrieve(
            "u1", "test", session_id="s1", query_embedding=[0.1] * 10, explain=True,
        )
        assert any(r.memory_id == "v1" for r in results)
        assert stats.vector_attempted is True
        assert stats.vector_hit is True
        assert stats.phase2_candidates == 1

    def test_keyword_failure_recorded_in_stats(self, retriever, mock_db):
        call_count = [0]
        def mock_execute(sql, params=None):
            call_count[0] += 1
            result = MagicMock()
            sql_str = str(sql)
            if "MATCH" in sql_str:
                raise Exception("Fulltext index error")
            result.fetchall.return_value = [
                MemRow("m1", "fallback", "semantic", 0.8, datetime(2026, 2, 26), None)
            ]
            return result

        mock_db.execute.side_effect = mock_execute
        results, stats = retriever.retrieve("u1", "test query", session_id="s1", explain=True)
        assert len(results) >= 1
        assert stats.keyword_attempted is True
        assert "Fulltext index error" in stats.keyword_error


# =============================================================================
# 2. Pipeline sandbox actually rejecting memories
# =============================================================================

class TestPipelineSandboxRejection:

    def test_sandbox_rejects_bad_memories(self):
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
            mock_sandbox.validate_memories.return_value = (False, None)
            MockSandbox.return_value = mock_sandbox

            result = run_typed_memory_pipeline(
                db_factory=lambda: mock_db, user_id="u1",
                messages=[{"role": "user", "content": "test"}],
                llm_client=mock_llm,
                config=MemoryGovernanceConfig(sandbox_enabled_types=("profile",)),
                query_for_sandbox="test query",
            )

        assert result.memories_extracted == 1
        assert result.memories_rejected == 1
        mock_store.create.assert_not_called()

    def test_sandbox_accepts_good_memories(self):
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
            mock_sandbox.validate_memories.return_value = (True, None)
            MockSandbox.return_value = mock_sandbox

            result = run_typed_memory_pipeline(
                db_factory=lambda: mock_db, user_id="u1",
                messages=[{"role": "user", "content": "test"}],
                llm_client=mock_llm,
                config=MemoryGovernanceConfig(sandbox_enabled_types=("profile",)),
                query_for_sandbox="test query",
            )

        assert result.memories_validated == 1
        assert result.memories_rejected == 0
        mock_store.create.assert_called_once()

    def test_non_sandbox_types_bypass_validation(self):
        mock_llm = MagicMock()
        mock_llm.chat_with_tools.return_value = {"content": json.dumps([
            {"type": "semantic", "content": "event happened", "confidence": 0.7},
        ])}
        mock_db = MagicMock()

        with patch("core.memory.typed_pipeline.MemoryStore") as MockStore, \
             patch("core.memory.typed_pipeline.MemorySandbox") as MockSandbox:
            mock_store = MagicMock()
            mock_store.create.side_effect = lambda m: m
            mock_store.list_active.return_value = []
            MockStore.return_value = mock_store
            mock_sandbox = MagicMock()
            mock_sandbox.validate_memories.return_value = (False, None)
            MockSandbox.return_value = mock_sandbox

            result = run_typed_memory_pipeline(
                db_factory=lambda: mock_db, user_id="u1",
                messages=[{"role": "user", "content": "test"}],
                llm_client=mock_llm,
                config=MemoryGovernanceConfig(sandbox_enabled_types=("profile",)),
                query_for_sandbox="test query",
            )

        assert result.memories_extracted == 1
        assert result.memories_rejected == 0
        mock_store.create.assert_called_once()

    def test_sandbox_failure_accepts_all(self):
        """Sandbox error → fail open, all memories accepted."""
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
                db_factory=lambda: mock_db, user_id="u1",
                messages=[{"role": "user", "content": "test"}],
                llm_client=mock_llm,
                config=MemoryGovernanceConfig(sandbox_enabled_types=("profile",)),
                query_for_sandbox="test query",
            )

        assert result.memories_extracted == 1
        mock_store.create.assert_called_once()


# =============================================================================
# 3. TOOL_RESULT TTL cleanup
# =============================================================================

class TestToolResultCleanup:

    def test_cleanup_tool_results_deletes_expired(self):
        config = MemoryGovernanceConfig(tool_result_ttl_hours=24)
        scheduler = GovernanceScheduler(MagicMock(), config)

        mock_session = MagicMock()
        mock_session.execute.return_value.rowcount = 5

        with patch.object(scheduler, "_db") as mock_db:
            mock_db.return_value.__enter__.return_value = mock_session
            mock_db.return_value.__exit__.return_value = None
            count = scheduler._cleanup_tool_results()

        assert count == 5
        call_args = mock_session.execute.call_args
        sql_text = str(call_args[0][0])
        assert "DELETE FROM memories" in sql_text
        params = call_args[0][1]
        assert params["mtype"] == "tool_result"
        assert params["ttl"] == 24

    def test_governance_cycle_includes_tool_result_cleanup(self):
        scheduler = GovernanceScheduler(MagicMock())

        with patch.object(scheduler, "_health_check"), \
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

    def test_db_contradiction_found(self):
        mock_db = MagicMock()
        mock_row = MagicMock()
        mock_row.memory_id = "old1"
        mock_row.content = "prefers tabs"
        mock_row.initial_confidence = 0.8
        mock_row.l2_dist = 0.1
        mock_db.execute.return_value.fetchone.return_value = mock_row

        observer = TypedObserver(
            store=MagicMock(), llm_client=None, embed_fn=None,
            contradiction_threshold=0.85, db_factory=lambda: mock_db,
        )
        result, _ = observer._find_contradiction(_mem(mid="new1", content="prefers spaces", embedding=[0.1] * 10))
        assert result is not None
        assert result.memory_id == "old1"

    def test_db_contradiction_not_found_when_distant(self):
        mock_db = MagicMock()
        mock_row = MagicMock()
        mock_row.memory_id = "old1"
        mock_row.content = "likes Go"
        mock_row.initial_confidence = 0.8
        mock_row.l2_dist = 5.0
        mock_db.execute.return_value.fetchone.return_value = mock_row

        observer = TypedObserver(
            store=MagicMock(), llm_client=None, embed_fn=None,
            contradiction_threshold=0.85, db_factory=lambda: mock_db,
        )
        result, _ = observer._find_contradiction(_mem(mid="new1", content="likes Rust", embedding=[0.1] * 10))
        assert result is None

    def test_db_error_propagates(self):
        mock_db = MagicMock()
        mock_db.execute.side_effect = Exception("DB connection lost")

        observer = TypedObserver(
            store=MagicMock(), llm_client=None, embed_fn=None,
            contradiction_threshold=0.85, db_factory=lambda: mock_db,
        )
        with pytest.raises(Exception, match="DB connection lost"):
            observer._find_contradiction(_mem(mid="new1", content="test", embedding=[0.1] * 10))

    def test_no_db_factory_skips_contradiction(self):
        store = MagicMock()
        observer = TypedObserver(
            store=store, llm_client=None, embed_fn=None,
            contradiction_threshold=0.85, db_factory=None,
        )
        result, _ = observer._find_contradiction(_mem(mid="new1", content="test", embedding=[0.5] * 10))
        assert result is None
        store.list_active.assert_not_called()


# =============================================================================
# 5. ProfileManager sort with None observed_at
# =============================================================================

class TestProfileSortWithNone:

    def test_sort_with_none_observed_at(self):
        store = MagicMock()
        store.list_active.return_value = [
            _mem(mid="m1", content="has date", initial_confidence=0.8, observed_at=datetime(2026, 2, 26)),
            _mem(mid="m2", content="no date", initial_confidence=0.9, observed_at=None),
            _mem(mid="m3", content="old date", initial_confidence=0.8, observed_at=datetime(2026, 1, 1)),
        ]
        mgr = ProfileManager(store)
        profile = mgr.get_profile("u1")
        assert "has date" in profile
        assert "no date" in profile
        assert "old date" in profile

    def test_sort_orders_by_confidence_then_recency(self):
        store = MagicMock()
        store.list_active.return_value = [
            _mem(mid="m1", content="low-conf-recent", initial_confidence=0.5, observed_at=datetime(2026, 2, 26)),
            _mem(mid="m2", content="high-conf-old", initial_confidence=0.9, observed_at=datetime(2026, 1, 1)),
            _mem(mid="m3", content="high-conf-recent", initial_confidence=0.9, observed_at=datetime(2026, 2, 26)),
        ]
        mgr = ProfileManager(store)
        profile = mgr.get_profile("u1")
        lines = profile.split("\n")[1:]
        assert "high-conf-recent" in lines[0]
        assert "high-conf-old" in lines[1]
        assert "low-conf-recent" in lines[2]


# =============================================================================
# 6. Store.supersede session_id propagation
# =============================================================================

class TestSupersedeSessionId:

    def test_supersede_preserves_session_id(self):
        mock_db = MagicMock()
        old_row = MagicMock(is_active=1)
        mock_db.query.return_value.filter_by.return_value.first.return_value = old_row

        store = MemoryStore(db_factory=lambda: mock_db)
        new_mem = _mem(mid="m2", content="new")
        new_mem.session_id = "sess_123"
        store.supersede("m1", new_mem)

        add_call = mock_db.add.call_args[0][0]
        assert add_call.session_id == "sess_123"


# =============================================================================
# 7. Confidence decay formula precision
# =============================================================================

class TestConfidenceDecayPrecision:

    def test_safe_exp_clamps(self):
        assert _safe_exp(-1000) == math.exp(-500)
        assert _safe_exp(1000) == math.exp(500)
        assert abs(_safe_exp(0) - 1.0) < 1e-10

    def test_decay_formula_matches_python(self):
        initial_conf = 0.9
        age_days = 60
        half_life = 30.0
        expected = initial_conf * math.exp(-age_days / half_life)
        assert abs(expected - 0.9 * math.exp(-2)) < 1e-10
        assert expected < 0.15

    def test_effective_confidence_method(self):
        """Memory.effective_confidence() computes query-time decay correctly."""
        m = _mem(initial_confidence=0.9, observed_at=datetime(2026, 1, 1))
        # Patch utcnow so we get deterministic result
        with patch("core.memory.types.datetime") as mock_dt:
            mock_dt.utcnow.return_value = datetime(2026, 1, 31)  # 30 days later
            eff = m.effective_confidence(half_life_days=30.0)
        expected = 0.9 * math.exp(-1.0)
        assert abs(eff - expected) < 0.01


# =============================================================================
# 8. Observer extract_candidates vs observe separation
# =============================================================================

class TestObserverExtractVsObserve:

    def test_extract_candidates_does_not_call_store(self):
        mock_store = MagicMock()
        mock_llm = MagicMock()
        mock_llm.chat_with_tools.return_value = {"content": json.dumps([
            {"type": "profile", "content": "likes Go", "confidence": 0.9},
        ])}
        observer = TypedObserver(store=mock_store, llm_client=mock_llm)
        candidates = observer.extract_candidates("u1", [{"role": "user", "content": "I like Go"}])
        assert len(candidates) == 1
        mock_store.create.assert_not_called()

    def test_observe_does_persist(self):
        mock_store = MagicMock()
        mock_store.create.side_effect = lambda m: m
        mock_llm = MagicMock()
        mock_llm.chat_with_tools.return_value = {"content": json.dumps([
            {"type": "profile", "content": "likes Go", "confidence": 0.9},
        ])}
        observer = TypedObserver(store=mock_store, llm_client=mock_llm)
        results, _ = observer.observe("u1", [{"role": "user", "content": "I like Go"}])
        assert len(results) == 1
        mock_store.create.assert_called_once()

    def test_persist_with_contradiction_check_public_api(self):
        mock_store = MagicMock()
        mock_store.create.side_effect = lambda m: m
        observer = TypedObserver(store=mock_store, llm_client=None)
        mem = _mem(content="test memory")
        result, _ = observer.persist_with_contradiction_check(mem)
        assert result.content == "test memory"
        mock_store.create.assert_called_once()


# =============================================================================
# 9. TieredLoader fallback — uses DI metrics
# =============================================================================

class TestTieredLoaderFallbackMetrics:

    def test_init_failure_increments_counter(self):
        from core.memory.tiered_loader import TieredMemoryLoader
        metrics = MemoryMetrics()
        with patch("core.memory.tiered_loader.MemoryStore") as MockStore:
            MockStore.side_effect = Exception("DB connection failed")
            loader = TieredMemoryLoader(lambda: MagicMock(), metrics=metrics)
            result = loader._ensure_initialized()
        assert result is False
        assert metrics._counters["tiered_loader_init_errors"] >= 1

    def test_l0_failure_increments_counter(self):
        from core.memory.tiered_loader import TieredMemoryLoader
        metrics = MemoryMetrics()
        loader = TieredMemoryLoader(lambda: MagicMock(), metrics=metrics)
        with patch.object(loader, "_ensure_initialized", return_value=True):
            loader._profile_mgr = MagicMock()
            loader._profile_mgr.get_profile.side_effect = Exception("Profile load failed")
            result = loader.load_l0("u1")
        assert result == ""
        assert metrics._counters["tiered_loader_l0_errors"] >= 1

    def test_l1_failure_increments_counter(self):
        from core.memory.tiered_loader import TieredMemoryLoader
        metrics = MemoryMetrics()
        loader = TieredMemoryLoader(lambda: MagicMock(), metrics=metrics)
        with patch.object(loader, "_ensure_initialized", return_value=True):
            loader._retriever = MagicMock()
            loader._retriever.retrieve.side_effect = Exception("Retrieval failed")
            result, _ = loader.load_l1("u1", "s1", "query")
        assert result == ""
        assert metrics._counters["tiered_loader_l1_errors"] >= 1


# =============================================================================
# 10. Sandbox explain stats
# =============================================================================

class TestSandboxExplainStats:

    def test_sandbox_exception_recorded_in_stats(self):
        from core.memory.sandbox import MemorySandbox
        mock_db = MagicMock()
        mock_db.execute.side_effect = Exception("Branch creation failed")
        sandbox = MemorySandbox(lambda: mock_db)
        result, stats = sandbox.validate_memories(
            user_id="u1", new_memories=[_mem(content="test")],
            query_text="test query", explain=True,
        )
        assert result is True
        assert stats is not None
        assert "Branch creation failed" in stats.error


# =============================================================================
# EXPLAIN ANALYZE Tests
# =============================================================================

class TestExplainAnalyze:

    def test_retriever_explain_returns_stats(self):
        mock_db = MagicMock()
        mock_db.execute.return_value.fetchall.return_value = [
            MemRow("m1", "test content", "semantic", 0.8, datetime(2026, 2, 26), None)
        ]
        retriever = MemoryRetriever(db_factory=lambda: mock_db)
        _, stats = retriever.retrieve("u1", "test", session_id="s1", explain=True)
        assert stats is not None
        assert stats.total_ms >= 0

    def test_retriever_no_explain_returns_none_stats(self):
        mock_db = MagicMock()
        mock_db.execute.return_value.fetchall.return_value = []
        retriever = MemoryRetriever(db_factory=lambda: mock_db)
        _, stats = retriever.retrieve("u1", "test", session_id="s1", explain=False)
        assert stats is None

    def test_observer_explain_returns_stats(self):
        mock_store = MagicMock()
        mock_store.create.side_effect = lambda m: m
        mock_llm = MagicMock()
        mock_llm.chat_with_tools.return_value = {"content": json.dumps([
            {"type": "profile", "content": "test", "confidence": 0.9},
        ])}
        observer = TypedObserver(store=mock_store, llm_client=mock_llm)
        _, stats = observer.observe("u1", [{"role": "user", "content": "test"}], explain=True)
        assert stats is not None
        assert stats.total_ms >= 0

    def test_pipeline_explain_returns_stats(self):
        result = run_typed_memory_pipeline(
            db_factory=lambda: MagicMock(), user_id="u1",
            messages=[{"role": "user", "content": "test"}],
            llm_client=None, explain=True,
        )
        assert result.stats is not None
        assert result.stats.total_ms >= 0
