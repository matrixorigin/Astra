"""Integration tests for branch merge with graph nodes/edges.

Tests the full branch lifecycle: create → store → reflect → merge → verify.
Uses real MatrixOne DB.
"""

from __future__ import annotations

import uuid

import pytest
from sqlalchemy import text


@pytest.fixture
def branch_user(db_session):
    """Create a unique user for branch tests and clean up after."""
    uid = f"__test_branch_{uuid.uuid4().hex[:8]}"
    yield uid
    # Cleanup: delete user data from all memory tables
    for table in ("memory_graph_edges", "memory_graph_nodes", "mem_memories",
                  "mem_edit_log", "mem_branches", "mem_user_state"):
        try:
            db_session.execute(text(f"DELETE FROM {table} WHERE user_id = :uid"), {"uid": uid})
        except Exception:
            pass
    db_session.commit()


@pytest.fixture
def backend(db_factory):
    """Create an EmbeddedBackend with test db_factory."""
    from unittest.mock import patch
    from mo_memory_mcp.server import EmbeddedBackend
    with patch.object(EmbeddedBackend, "__init__", lambda self, **kw: None):
        b = EmbeddedBackend.__new__(EmbeddedBackend)
        b._db_factory = db_factory
        b._engine = None
        b._embed_client = None
        from core.memory.factory import create_editor, create_memory_service
        b._create_service = create_memory_service
        b._create_editor = create_editor
        # Instance vars — each test gets a fresh backend instance, no shared state.
        b._active_branches = {}
        b._branch_factory_cache = {}
        b._cooldown_cache = {}
        yield b


class TestBranchMergeGraph:
    """Test that merge includes graph nodes and edges."""

    def test_merge_includes_graph_nodes(self, backend, branch_user, db_session):
        """Graph nodes created on branch should appear in main after merge."""
        uid = branch_user

        # 1. Create branch
        result = backend.branch_create(uid, "graph_test", None, None)
        assert "error" not in result
        branch_db = result["branch_db"]

        # 2. Insert a graph node directly into branch DB
        node_id = uuid.uuid4().hex[:32]
        db_session.execute(text(f"""
            INSERT INTO `{branch_db}`.memory_graph_nodes
            (node_id, user_id, node_type, content, confidence, trust_tier, importance, is_active, created_at)
            VALUES (:nid, :uid, 'scene', 'test insight from branch', 0.8, 'T2', 0.5, 1, NOW())
        """), {"nid": node_id, "uid": uid})
        db_session.commit()

        # 3. Verify node exists in branch but NOT in main
        branch_count = db_session.execute(text(
            f"SELECT COUNT(*) FROM `{branch_db}`.memory_graph_nodes WHERE node_id = :nid"
        ), {"nid": node_id}).scalar()
        assert branch_count == 1

        main_count = db_session.execute(text(
            "SELECT COUNT(*) FROM memory_graph_nodes WHERE node_id = :nid"
        ), {"nid": node_id}).scalar()
        assert main_count == 0

        # 4. Merge
        merge_result = backend.branch_merge(uid, "graph_test", "append")
        assert "error" not in merge_result
        assert merge_result["graph_nodes_merged"] == 1

        # 5. Verify node now in main
        main_count = db_session.execute(text(
            "SELECT COUNT(*) FROM memory_graph_nodes WHERE node_id = :nid"
        ), {"nid": node_id}).scalar()
        assert main_count == 1

        # Verify all fields
        row = db_session.execute(text(
            "SELECT * FROM memory_graph_nodes WHERE node_id = :nid"
        ), {"nid": node_id}).fetchone()
        assert row.user_id == uid
        assert row.node_type == "scene"
        assert row.content == "test insight from branch"
        assert row.is_active == 1

        # Cleanup branch
        backend.branch_delete(uid, "graph_test")

    def test_merge_includes_graph_edges(self, backend, branch_user, db_session):
        """Graph edges created on branch should appear in main after merge."""
        uid = branch_user

        # 1. Create branch
        result = backend.branch_create(uid, "edge_test", None, None)
        assert "error" not in result
        branch_db = result["branch_db"]

        # 2. Insert two nodes + an edge in branch
        n1 = uuid.uuid4().hex[:32]
        n2 = uuid.uuid4().hex[:32]
        for nid in (n1, n2):
            db_session.execute(text(f"""
                INSERT INTO `{branch_db}`.memory_graph_nodes
                (node_id, user_id, node_type, content, confidence, trust_tier, importance, is_active, created_at)
                VALUES (:nid, :uid, 'fact', 'node content', 0.8, 'T3', 0.0, 1, NOW())
            """), {"nid": nid, "uid": uid})

        db_session.execute(text(f"""
            INSERT INTO `{branch_db}`.memory_graph_edges
            (source_id, target_id, edge_type, weight, user_id)
            VALUES (:s, :t, 'related', 1.0, :uid)
        """), {"s": n1, "t": n2, "uid": uid})
        db_session.commit()

        # 3. Merge
        merge_result = backend.branch_merge(uid, "edge_test", "append")
        assert "error" not in merge_result
        assert merge_result["graph_nodes_merged"] == 2
        assert merge_result["graph_edges_merged"] == 1

        # 4. Verify edge in main
        edge = db_session.execute(text(
            "SELECT * FROM memory_graph_edges WHERE source_id = :s AND target_id = :t AND edge_type = 'related'"
        ), {"s": n1, "t": n2}).fetchone()
        assert edge is not None
        assert edge.user_id == uid
        assert edge.weight == 1.0

        backend.branch_delete(uid, "edge_test")

    def test_merge_skips_existing_nodes(self, backend, branch_user, db_session):
        """Nodes already in main should not be duplicated."""
        uid = branch_user

        # 1. Insert a node in main
        node_id = uuid.uuid4().hex[:32]
        db_session.execute(text("""
            INSERT INTO memory_graph_nodes
            (node_id, user_id, node_type, content, confidence, trust_tier, importance, is_active, created_at)
            VALUES (:nid, :uid, 'fact', 'original content', 0.8, 'T3', 0.0, 1, NOW())
        """), {"nid": node_id, "uid": uid})
        db_session.commit()

        # 2. Create branch (will fork the node)
        result = backend.branch_create(uid, "dup_test", None, None)
        assert "error" not in result
        branch_db = result["branch_db"]

        # 3. Modify the node on branch
        db_session.execute(text(f"""
            UPDATE `{branch_db}`.memory_graph_nodes
            SET content = 'modified on branch'
            WHERE node_id = :nid
        """), {"nid": node_id})
        db_session.commit()

        # 4. Merge — should skip (node_id already exists in main)
        merge_result = backend.branch_merge(uid, "dup_test", "append")
        assert merge_result["graph_nodes_merged"] == 0

        # 5. Main content unchanged
        row = db_session.execute(text(
            "SELECT content FROM memory_graph_nodes WHERE node_id = :nid"
        ), {"nid": node_id}).fetchone()
        assert row.content == "original content"

        backend.branch_delete(uid, "dup_test")

    def test_merge_skips_existing_edges(self, backend, branch_user, db_session):
        """Edges already in main should not be duplicated."""
        uid = branch_user

        # 1. Insert nodes + edge in main
        n1 = uuid.uuid4().hex[:32]
        n2 = uuid.uuid4().hex[:32]
        for nid in (n1, n2):
            db_session.execute(text("""
                INSERT INTO memory_graph_nodes
                (node_id, user_id, node_type, content, confidence, trust_tier, importance, is_active, created_at)
                VALUES (:nid, :uid, 'fact', 'content', 0.8, 'T3', 0.0, 1, NOW())
            """), {"nid": nid, "uid": uid})
        db_session.execute(text("""
            INSERT INTO memory_graph_edges
            (source_id, target_id, edge_type, weight, user_id)
            VALUES (:s, :t, 'related', 1.0, :uid)
        """), {"s": n1, "t": n2, "uid": uid})
        db_session.commit()

        # 2. Create branch (forks the edge)
        result = backend.branch_create(uid, "dup_edge_test", None, None)
        assert "error" not in result

        # 3. Merge — edge already exists, should skip
        merge_result = backend.branch_merge(uid, "dup_edge_test", "append")
        assert merge_result["graph_edges_merged"] == 0

        # 4. Only one edge in main
        count = db_session.execute(text(
            "SELECT COUNT(*) FROM memory_graph_edges WHERE source_id = :s AND target_id = :t"
        ), {"s": n1, "t": n2}).scalar()
        assert count == 1

        backend.branch_delete(uid, "dup_edge_test")

    def test_empty_branch_merge_returns_zero(self, backend, branch_user, db_session):
        """Merging a branch with no changes returns zero counts."""
        uid = branch_user

        result = backend.branch_create(uid, "empty_test", None, None)
        assert "error" not in result

        merge_result = backend.branch_merge(uid, "empty_test", "append")
        assert merge_result["merged"] == 0
        assert merge_result["graph_nodes_merged"] == 0
        assert merge_result["graph_edges_merged"] == 0

        backend.branch_delete(uid, "empty_test")


class TestBranchFactoryCacheIntegration:
    """DB-level tests for branch factory caching."""

    def test_cached_factory_reads_branch_data(self, backend, branch_user, db_session):
        """Cached factory must actually connect to branch DB and read data."""
        uid = branch_user

        # 1. Create branch and store a memory on it
        result = backend.branch_create(uid, "cache_test", None, None)
        assert "error" not in result
        branch_db = result["branch_db"]

        # Insert a memory directly into branch DB
        mid = uuid.uuid4().hex[:32]
        db_session.execute(text(f"""
            INSERT INTO `{branch_db}`.mem_memories
            (memory_id, user_id, content, memory_type, initial_confidence,
             trust_tier, source_event_ids, is_active, observed_at, created_at, updated_at)
            VALUES (:mid, :uid, 'branch data', 'semantic', 0.9, 'T1', '[]', 1, NOW(), NOW(), NOW())
        """), {"mid": mid, "uid": uid})
        db_session.commit()

        # 2. Checkout branch — first call populates cache
        backend.branch_checkout(uid, "cache_test")
        factory1 = backend._branch_db_factory(uid)

        # 3. Second call should hit cache — same object
        factory2 = backend._branch_db_factory(uid)
        assert factory1 is factory2

        # 4. Cached factory can actually read the branch data
        with factory1() as sess:
            row = sess.execute(text(
                "SELECT content FROM mem_memories WHERE memory_id = :mid"
            ), {"mid": mid}).fetchone()
            assert row is not None
            assert row.content == "branch data"

        # 5. Verify main does NOT have this memory
        main_count = db_session.execute(text(
            "SELECT COUNT(*) FROM mem_memories WHERE memory_id = :mid"
        ), {"mid": mid}).scalar()
        assert main_count == 0

        backend.branch_checkout(uid, "main")
        backend.branch_delete(uid, "cache_test")

    def test_cache_invalidated_on_checkout(self, backend, branch_user, db_session):
        """Switching branches must invalidate the old branch's cache entry."""
        uid = branch_user

        # Create two branches
        r1 = backend.branch_create(uid, "br_a", None, None)
        r2 = backend.branch_create(uid, "br_b", None, None)
        assert "error" not in r1
        assert "error" not in r2

        # Checkout br_a and populate the factory cache
        backend.branch_checkout(uid, "br_a")
        backend._branch_db_factory(uid)
        assert (uid, "br_a") in backend._branch_factory_cache

        # Checkout br_b — _set_active_branch must evict the br_a cache entry
        backend.branch_checkout(uid, "br_b")
        assert (uid, "br_a") not in backend._branch_factory_cache

        backend.branch_checkout(uid, "main")
        backend.branch_delete(uid, "br_a")
        backend.branch_delete(uid, "br_b")


class TestMergeRowcountIntegration:
    """DB-level test that merge returns correct inserted count via rowcount."""

    def test_merge_inserted_count_matches_actual(self, backend, branch_user, db_session):
        """merge 'inserted' field must equal actual new rows in main."""
        uid = branch_user

        # 1. Create branch
        result = backend.branch_create(uid, "rc_test", None, None)
        assert "error" not in result
        branch_db = result["branch_db"]

        # 2. Insert 3 memories into branch
        mids = []
        for i in range(3):
            mid = uuid.uuid4().hex[:32]
            mids.append(mid)
            db_session.execute(text(f"""
                INSERT INTO `{branch_db}`.mem_memories
                (memory_id, user_id, content, memory_type, initial_confidence,
                 trust_tier, source_event_ids, is_active, observed_at, created_at, updated_at)
                VALUES (:mid, :uid, :content, 'semantic', 0.9, 'T1', '[]', 1, NOW(), NOW(), NOW())
            """), {"mid": mid, "uid": uid, "content": f"rowcount test {i}"})
        db_session.commit()

        # 3. Count main before merge
        before = db_session.execute(text(
            "SELECT COUNT(*) FROM mem_memories WHERE user_id = :uid AND is_active"
        ), {"uid": uid}).scalar()

        # 4. Merge
        merge_result = backend.branch_merge(uid, "rc_test", "append")
        assert "error" not in merge_result

        # 5. Count main after merge
        after = db_session.execute(text(
            "SELECT COUNT(*) FROM mem_memories WHERE user_id = :uid AND is_active"
        ), {"uid": uid}).scalar()

        # 6. Verify: reported inserted == actual new rows
        actual_new = after - before
        assert merge_result["inserted"] == actual_new
        assert merge_result["merged"] == actual_new  # merged = inserted + replaced; replaced=0 here
        assert actual_new == 3

        backend.branch_delete(uid, "rc_test")


class TestDetectConflictsIntegration:
    """DB-level test for _detect_conflicts with real cosine_similarity."""

    def test_detects_similar_memories(self, backend, branch_user, db_session):
        """Semantically similar memories (cosine > 0.9) must be detected as conflicts;
        unrelated memories (cosine < 0.9) must not.

        Uses two distinct sentence pairs to exercise the threshold:
        - "Python is a great language" vs "Python is an excellent language" → cosine ≈ 0.97 (conflict)
        - "Python is a great language" vs "The weather in Tokyo is sunny" → cosine ≈ 0.05 (no conflict)
        """
        uid = branch_user

        from core.embedding.client import EmbeddingClient
        ec = EmbeddingClient(provider="local", model="all-MiniLM-L6-v2", dim=384, api_key="", base_url=None)

        # Two semantically similar Python sentences — cosine should be well above 0.9
        emb_main = ec.embed("Python is a great programming language")
        emb_similar = ec.embed("Python is an excellent programming language")
        # Completely unrelated sentence — cosine should be well below 0.9
        emb_different = ec.embed("The weather in Tokyo is sunny today")

        def vec(e):
            return f"[{','.join(str(x) for x in e)}]"

        # Sanity-check the embeddings before inserting into DB.
        # If the local model produces unexpected similarity, the test should fail
        # loudly here rather than giving a false pass later.
        import math
        def cosine(a, b):
            dot = sum(x * y for x, y in zip(a, b))
            na = math.sqrt(sum(x * x for x in a))
            nb = math.sqrt(sum(x * x for x in b))
            return dot / (na * nb) if na and nb else 0.0

        sim_score = cosine(emb_main, emb_similar)
        diff_score = cosine(emb_main, emb_different)
        assert sim_score > 0.9, (
            f"Expected similar sentences to have cosine > 0.9, got {sim_score:.3f}. "
            "Check the local embedding model."
        )
        assert diff_score < 0.9, (
            f"Expected unrelated sentences to have cosine < 0.9, got {diff_score:.3f}. "
            "Check the local embedding model."
        )

        # 1. Insert main memory (similar embedding)
        main_mid = uuid.uuid4().hex[:32]
        db_session.execute(text("""
            INSERT INTO mem_memories
            (memory_id, user_id, content, memory_type, initial_confidence,
             trust_tier, embedding, source_event_ids, is_active, observed_at, created_at, updated_at)
            VALUES (:mid, :uid, 'Python is a great programming language', 'semantic', 0.9,
                    'T1', :emb, '[]', 1, NOW(), NOW(), NOW())
        """), {"mid": main_mid, "uid": uid, "emb": vec(emb_main)})
        db_session.commit()

        # 2. Create branch
        result = backend.branch_create(uid, "conflict_test", None, None)
        assert "error" not in result
        branch_db = result["branch_db"]

        # 3. Insert two branch memories: one semantically similar, one unrelated
        similar_mid = uuid.uuid4().hex[:32]
        different_mid = uuid.uuid4().hex[:32]
        db_session.execute(text(f"""
            INSERT INTO `{branch_db}`.mem_memories
            (memory_id, user_id, content, memory_type, initial_confidence,
             trust_tier, embedding, source_event_ids, is_active, observed_at, created_at, updated_at)
            VALUES (:mid, :uid, 'Python is an excellent programming language', 'semantic', 0.9,
                    'T1', :emb, '[]', 1, NOW(), NOW(), NOW())
        """), {"mid": similar_mid, "uid": uid, "emb": vec(emb_similar)})
        db_session.execute(text(f"""
            INSERT INTO `{branch_db}`.mem_memories
            (memory_id, user_id, content, memory_type, initial_confidence,
             trust_tier, embedding, source_event_ids, is_active, observed_at, created_at, updated_at)
            VALUES (:mid, :uid, 'The weather in Tokyo is sunny today', 'semantic', 0.9,
                    'T1', :emb, '[]', 1, NOW(), NOW(), NOW())
        """), {"mid": different_mid, "uid": uid, "emb": vec(emb_different)})
        db_session.commit()

        # 4. Detect conflicts — must use the same threshold as branch_merge
        from mo_memory_mcp.server import EmbeddedBackend
        assert backend._CONFLICT_COSINE_THRESHOLD == EmbeddedBackend._CONFLICT_COSINE_THRESHOLD
        conflicts = backend._detect_conflicts(branch_db, uid, [similar_mid, different_mid])

        # similar_mid: cosine ≈ 0.97 > 0.9 → conflict
        assert similar_mid in conflicts
        # different_mid: cosine ≈ 0.05 < 0.9 → no conflict
        assert different_mid not in conflicts

        backend.branch_delete(uid, "conflict_test")
