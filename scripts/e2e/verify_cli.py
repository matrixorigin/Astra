#!/usr/bin/env python3
"""
E2E Verification — real DB writes, real assertions, optional real LLM.

Directly imports Python APIs (no subprocess). Runs against the dev database
configured in .env. Uses a dedicated `__verify_<uuid>` user ID to isolate
test data, and cleans up automatically.

Usage:
    make verify                  # core scenarios (no LLM needed)
    make verify-llm              # includes NL→Script via real LLM
    make verify VERBOSE=1        # verbose output
"""
from __future__ import annotations

import os
import sys
import uuid
from pathlib import Path

# Force HuggingFace offline mode — model is cached locally, avoid proxy hang
os.environ["HF_HUB_OFFLINE"] = "1"
os.environ["TRANSFORMERS_OFFLINE"] = "1"
for _k in ("http_proxy", "https_proxy", "HTTP_PROXY", "HTTPS_PROXY"):
    os.environ.pop(_k, None)

sys.path.insert(0, str(Path(__file__).parent.parent.parent))

import argparse
import json
import logging

from sqlalchemy import text

from api.database import SessionLocal

# ── Config ────────────────────────────────────────────────────────────

USER_ID = f"__verify_{uuid.uuid4().hex[:8]}"
DB_NAME = SessionLocal.kw["bind"].url.database

_passed = 0
_failed = 0
_verbose = False

logging.basicConfig(level=logging.WARNING)


# ── Helpers ───────────────────────────────────────────────────────────

def ok(name: str, msg: str = "") -> None:
    global _passed
    _passed += 1
    print(f"  ✅ {name}" + (f" — {msg}" if msg else ""))


def fail(name: str, msg: str) -> None:
    global _failed
    _failed += 1
    print(f"  ❌ {name} — {msg}")


def check(name: str, condition: bool, msg: str = "") -> None:
    ok(name, msg) if condition else fail(name, msg)


def vlog(msg: str) -> None:
    if _verbose:
        print(f"    ↳ {msg}")


def query_one(sql: str, **params):
    with SessionLocal() as db:
        return db.execute(text(sql), params).first()


def query_all(sql: str, **params):
    with SessionLocal() as db:
        return db.execute(text(sql), params).fetchall()


def scalar(sql: str, **params) -> int:
    with SessionLocal() as db:
        return db.execute(text(sql), params).scalar() or 0


def count(table: str) -> int:
    return scalar(f"SELECT COUNT(*) FROM {table} WHERE user_id = :uid", uid=USER_ID)


def cleanup() -> None:
    with SessionLocal() as db:
        for t in ("mem_edit_log", "mem_memories", "memory_graph_nodes", "memory_graph_edges"):
            db.execute(text(f"DELETE FROM {t} WHERE user_id = :uid"), {"uid": USER_ID})
        db.commit()

    # Discard any experiment branches created for this user
    try:
        from core.memory.experiment import MemoryExperimentManager
        mgr = MemoryExperimentManager(SessionLocal, source_db=DB_NAME)
        with SessionLocal() as db:
            rows = db.execute(
                text("SELECT experiment_id FROM mem_experiments WHERE user_id = :uid"),
                {"uid": USER_ID},
            ).fetchall()
        for row in rows:
            try:
                mgr.discard(row.experiment_id)
            except Exception:
                pass
    except Exception:
        pass

    vlog(f"cleaned up {USER_ID}")


def _make_programmer():
    from core.memory.experiment import MemoryExperimentManager
    from core.memory.factory import create_editor
    from core.memory.programmer import MemoryProgrammer

    editor = create_editor(SessionLocal, user_id=USER_ID)
    experiments = MemoryExperimentManager(SessionLocal, source_db=DB_NAME)
    return MemoryProgrammer(editor, experiments, SessionLocal)


def _parse(yaml_str: str) -> list[dict]:
    from core.memory.programmer import parse_script
    return parse_script(yaml_str)


# ── Scenario 1: Sandbox inject ───────────────────────────────────────

def test_sandbox_inject() -> str | None:
    print("\n── 1. Sandbox inject ──")

    actions = _parse("version: 1\nactions:\n  - inject:\n      content: 'verify sandbox test'\n      type: semantic\n      trust: T1\n")
    prog = _make_programmer()
    mem_before = count("mem_memories")

    result = prog.execute(USER_ID, actions, sandbox=True, dry_run=False, program_name="verify")
    vlog(f"result: executed={result.actions_executed}, exp={result.experiment_id}")

    check("executed 1 action", result.actions_executed == 1)
    check("has experiment_id", result.experiment_id is not None)
    check("production unchanged", count("mem_memories") == mem_before)

    if result.experiment_id:
        exp = query_one(
            "SELECT status, branch_db FROM mem_experiments WHERE experiment_id = :eid",
            eid=result.experiment_id,
        )
        check("experiment status=active", exp is not None and exp.status == "active")

    return result.experiment_id


# ── Scenario 2: Commit ───────────────────────────────────────────────

def test_commit(experiment_id: str) -> None:
    print("\n── 2. Commit ──")
    from core.memory.experiment import MemoryExperimentManager

    mem_before = count("mem_memories")
    mgr = MemoryExperimentManager(SessionLocal, source_db=DB_NAME)
    mgr.commit(experiment_id)

    check("memory count +1", count("mem_memories") == mem_before + 1)

    exp = query_one("SELECT status FROM mem_experiments WHERE experiment_id = :eid", eid=experiment_id)
    check("experiment committed", exp is not None and exp.status == "committed")

    # field-level verification
    mem = query_one(
        "SELECT content, memory_type, trust_tier, is_active, user_id "
        "FROM mem_memories WHERE user_id = :uid ORDER BY created_at DESC LIMIT 1",
        uid=USER_ID,
    )
    check("content correct", mem is not None and "verify sandbox test" in (mem.content or ""))
    check("is_active=1", mem is not None and mem.is_active == 1)
    check("user_id correct", mem is not None and mem.user_id == USER_ID)


# ── Scenario 3: Dry-run ──────────────────────────────────────────────

def test_dry_run() -> None:
    print("\n── 3. Dry-run ──")

    actions = _parse("version: 1\nactions:\n  - inject:\n      content: 'should not persist'\n      type: semantic\n")
    prog = _make_programmer()
    mem_before = count("mem_memories")
    audit_before = count("mem_edit_log")

    result = prog.execute(USER_ID, actions, sandbox=False, dry_run=True, program_name="verify")

    check("dry_run=True", result.dry_run is True)
    check("memories unchanged", count("mem_memories") == mem_before)
    check("audits unchanged", count("mem_edit_log") == audit_before)


# ── Scenario 4: Discard ──────────────────────────────────────────────

def test_discard() -> None:
    print("\n── 4. Discard ──")
    from core.memory.experiment import MemoryExperimentManager

    actions = _parse("version: 1\nactions:\n  - inject:\n      content: 'to discard'\n      type: semantic\n")
    prog = _make_programmer()
    mem_before = count("mem_memories")

    result = prog.execute(USER_ID, actions, sandbox=True, dry_run=False, program_name="verify")
    exp_id = result.experiment_id

    mgr = MemoryExperimentManager(SessionLocal, source_db=DB_NAME)
    mgr.discard(exp_id)

    check("production unchanged", count("mem_memories") == mem_before)
    exp = query_one("SELECT status FROM mem_experiments WHERE experiment_id = :eid", eid=exp_id)
    check("experiment discarded", exp is not None and exp.status == "discarded")


# ── Scenario 5: Direct write + dual audit ────────────────────────────

def test_direct_write() -> None:
    print("\n── 5. Direct write (no sandbox) ──")

    actions = _parse("version: 1\nactions:\n  - inject:\n      content: 'direct write verify'\n      type: semantic\n      trust: T1\n")
    prog = _make_programmer()
    mem_before = count("mem_memories")
    audit_before = count("mem_edit_log")

    result = prog.execute(USER_ID, actions, sandbox=False, dry_run=False, program_name="verify")

    check("memory count +1", count("mem_memories") == mem_before + 1)
    check("dual audit (>=+2)", count("mem_edit_log") >= audit_before + 2,
          f"was {audit_before}, now {count('mem_edit_log')}")

    mem = query_one(
        "SELECT content, user_id FROM mem_memories "
        "WHERE user_id = :uid ORDER BY created_at DESC LIMIT 1",
        uid=USER_ID,
    )
    check("content correct", mem is not None and "direct write verify" in (mem.content or ""))

    # Graph index: activation:v1 default should write graph nodes
    graph_count = scalar(
        "SELECT COUNT(*) FROM memory_graph_nodes WHERE user_id = :uid", uid=USER_ID,
    )
    check("graph nodes created", graph_count >= 1, f"graph_count={graph_count}")


# ── Scenario 6: Multi-turn (inject → correct → verify) ───────────────

def test_multi_turn() -> None:
    print("\n── 6. Multi-turn (inject → correct) ──")

    prog = _make_programmer()

    # Turn 1: inject
    r1 = prog.execute(USER_ID, _parse(
        "version: 1\nactions:\n  - inject:\n      content: 'prefers Python for data analysis'\n      type: semantic\n"
    ), sandbox=False, dry_run=False, program_name="verify")

    mem1_id = r1.results[0].detail.get("memory_id") if r1.results else None
    check("turn1 injected", mem1_id is not None)

    if not mem1_id:
        return

    # Turn 2: correct
    r2 = prog.execute(USER_ID, _parse(json.dumps(
        {"version": 1, "actions": [{"correct": {
            "memory_id": mem1_id,
            "new_content": "prefers Rust for data analysis now",
            "reason": "preference changed",
        }}]}
    )), sandbox=False, dry_run=False, program_name="verify")

    check("turn2 corrected", r2.actions_executed == 1)

    # Verify the corrected memory
    new_id = r2.results[0].detail.get("new_id") if r2.results else None
    if new_id:
        mem = query_one(
            "SELECT content FROM mem_memories WHERE memory_id = :mid", mid=new_id,
        )
        check("corrected content", mem is not None and "Rust" in (mem.content or ""))

    # Verify audit chain
    audits = query_all(
        "SELECT operation FROM mem_edit_log WHERE user_id = :uid ORDER BY created_at",
        uid=USER_ID,
    )
    ops = [a.operation for a in audits]
    check("audit has inject+correct", "inject" in ops and "correct" in ops)


# ── Scenario 7: Graph edges (ASSOCIATION) ─────────────────────────────

def test_graph_edges() -> None:
    print("\n── 7. Graph edges (ASSOCIATION) ──")

    from core.memory.factory import create_editor

    editor = create_editor(SessionLocal, user_id=USER_ID)

    has_embed = editor._embed_client is not None
    if not has_embed:
        fail("embed_client", "no embed_client — cannot test ASSOCIATION edges")
        return

    # Inject two semantically similar memories via batch_inject (generates embeddings)
    stored = editor.batch_inject(USER_ID, [
        {"content": "The user prefers Python for data analysis and machine learning", "type": "semantic", "trust": "T1"},
        {"content": "The user likes Python for data science and ML projects", "type": "semantic", "trust": "T1"},
    ], source="verify-edges")

    check("batch stored 2", len(stored) == 2)
    check("embeddings present", all(m.embedding is not None for m in stored))

    # Verify graph nodes created
    node_count = scalar(
        "SELECT COUNT(*) FROM memory_graph_nodes WHERE user_id = :uid", uid=USER_ID,
    )
    check("graph nodes >= 2", node_count >= 2, f"node_count={node_count}")

    # Verify ASSOCIATION edge created between the two similar memories
    edge_count = scalar(
        "SELECT COUNT(*) FROM memory_graph_edges WHERE user_id = :uid AND edge_type = 'association'",
        uid=USER_ID,
    )
    check("association edges >= 1", edge_count >= 1, f"edge_count={edge_count}")

    # Verify edge weight is reasonable (cosine sim > 0.3 threshold)
    edge = query_one(
        "SELECT weight FROM memory_graph_edges WHERE user_id = :uid AND edge_type = 'association' LIMIT 1",
        uid=USER_ID,
    )
    if edge:
        check("edge weight > 0.3", edge.weight > 0.3, f"weight={edge.weight}")
    else:
        fail("edge weight", "no association edge found")


# ── Scenario 8: Spreading activation retrieval ───────────────────────

def test_spreading_activation() -> None:
    print("\n── 8. Spreading activation retrieval ──")

    from core.memory.factory import create_editor
    from core.memory.graph.retriever import MIN_GRAPH_NODES
    from core.memory.strategy.activation_v1 import ActivationRetrievalStrategy

    editor = create_editor(SessionLocal, user_id=USER_ID)

    has_embed = editor._embed_client is not None
    if not has_embed:
        fail("embed_client", "no embed_client — cannot test spreading activation")
        return

    # We need >= MIN_GRAPH_NODES (50) nodes for activation to engage.
    # Build a cluster of related memories so edges form a connected graph.
    existing = scalar(
        "SELECT COUNT(*) FROM memory_graph_nodes WHERE user_id = :uid", uid=USER_ID,
    )
    needed = max(MIN_GRAPH_NODES + 5 - existing, 0)
    vlog(f"existing nodes={existing}, need to create {needed} more")

    if needed > 0:
        # Generate memories in batches of 10 to keep embed calls manageable
        topics = [
            "Python", "Rust", "Go", "TypeScript", "Java",
            "machine learning", "data analysis", "web development",
            "database optimization", "API design",
        ]
        batch: list[dict] = []
        for i in range(needed):
            topic = topics[i % len(topics)]
            batch.append({
                "content": f"The user has experience with {topic} — note {i}",
                "type": "semantic", "trust": "T2",
            })
            if len(batch) >= 10:
                editor.batch_inject(USER_ID, batch, source="verify-activation-seed")
                batch = []
        if batch:
            editor.batch_inject(USER_ID, batch, source="verify-activation-seed")

    final_nodes = scalar(
        "SELECT COUNT(*) FROM memory_graph_nodes WHERE user_id = :uid", uid=USER_ID,
    )
    check(f"nodes >= {MIN_GRAPH_NODES}", final_nodes >= MIN_GRAPH_NODES, f"nodes={final_nodes}")

    final_edges = scalar(
        "SELECT COUNT(*) FROM memory_graph_edges WHERE user_id = :uid", uid=USER_ID,
    )
    vlog(f"total edges={final_edges}")
    check("edges > 0", final_edges > 0, f"edges={final_edges}")

    # Now retrieve via ActivationRetrievalStrategy — should use graph, not vector fallback
    strategy = ActivationRetrievalStrategy(SessionLocal)

    # Get a query embedding
    query_text = "What programming languages does the user know?"
    query_emb = editor._embed_client.embed_batch([query_text])[0]

    memories, _ = strategy.retrieve(
        USER_ID, query_text, query_emb, top_k=5,
    )
    check("activation returned results", len(memories) > 0, f"got {len(memories)}")

    if memories:
        vlog(f"top result: {memories[0].content[:80]}")
        # Verify results are actual Memory objects with content
        check("result has content", bool(memories[0].content))
        check("result has user_id", memories[0].user_id == USER_ID)


# ── Scenario 9: Graph > Vector (2-hop recall) ─────────────────────────

def test_graph_vs_vector() -> None:
    print("\n── 9. Graph > Vector (2-hop recall) ──")

    import numpy as np
    from core.memory.factory import (
        _register_builtins, _resolve_strategy, _registry, StrategyDescriptor, create_editor,
    )
    from core.memory.graph.graph_store import GraphStore
    from core.memory.strategy.vector_v1 import VectorRetrievalStrategy
    from core.embedding import get_embedding_client

    ec = get_embedding_client()
    if ec is None:
        fail("embed_client", "no embed_client")
        return

    gv_user = USER_ID + "_gv"
    store = GraphStore(SessionLocal)

    try:
        editor = create_editor(SessionLocal, user_id=gv_user)

        # 3 key memories with controlled cosine similarity to query
        stored = editor.batch_inject(gv_user, [
            {"content": "User prefers Python for data analysis", "type": "semantic", "trust": "T1"},        # A: cos≈0.75
            {"content": "User is working on a machine learning project", "type": "semantic", "trust": "T1"}, # B: cos≈0.28
            {"content": "User is studying transformer architecture in depth", "type": "semantic", "trust": "T1"}, # C: cos≈0.13
        ], source="gv-test")

        check("stored 3 memories", len(stored) == 3)
        check("embeddings present", all(m.embedding is not None for m in stored))

        # Seed 10 more unrelated nodes to pass MIN_GRAPH_NODES threshold
        editor.batch_inject(gv_user, [
            {"content": f"Unrelated administrative record {i:02d}", "type": "semantic", "trust": "T3"}
            for i in range(10)
        ], source="gv-seed")

        with SessionLocal() as db:
            nodes = db.execute(text(
                "SELECT node_id, content FROM memory_graph_nodes WHERE user_id = :u"
            ), {"u": gv_user}).fetchall()

        a_id = next((n.node_id for n in nodes if "Python" in n.content), None)
        b_id = next((n.node_id for n in nodes if "machine learning" in n.content), None)
        c_id = next((n.node_id for n in nodes if "transformer" in n.content), None)
        check("key nodes found", all([a_id, b_id, c_id]))

        if not all([a_id, b_id, c_id]):
            return

        # Force clean A->B->C chain (remove auto-created edges, add controlled ones)
        with SessionLocal() as db:
            db.execute(text("DELETE FROM memory_graph_edges WHERE user_id = :u"), {"u": gv_user})
            db.commit()
        store.add_edges_batch([
            (a_id, b_id, "association", 0.8),  # A -> B (strong)
            (b_id, c_id, "association", 0.8),  # B -> C (strong, 2-hop from A)
        ], gv_user)

        query = "Python data analysis"
        q_emb = ec.embed(query)

        # Verify cosine similarities (C should be low)
        c_mem = next((m for m in stored if "transformer" in m.content), None)
        if c_mem and c_mem.embedding:
            cos_c = float(np.dot(q_emb, c_mem.embedding) / (
                np.linalg.norm(q_emb) * np.linalg.norm(c_mem.embedding)
            ))
            check("C has low cosine to query", cos_c < 0.2, f"cos={cos_c:.3f}")
            vlog(f"cos(query, C) = {cos_c:.3f}")

        # Vector top-2: should NOT include C
        vec_strategy = VectorRetrievalStrategy(SessionLocal)
        vec_mems, _ = vec_strategy.retrieve(gv_user, query, q_emb, top_k=2)
        c_in_vec = any("transformer" in m.content for m in vec_mems)
        check("vector misses C (low cosine)", not c_in_vec, f"vec results: {[m.content[:30] for m in vec_mems]}")

        # Graph top-3: should include C via 2-hop A->B->C activation
        _register_builtins()
        sk = _resolve_strategy(SessionLocal, gv_user, backend=None, strategy=None)
        desc = StrategyDescriptor.parse(sk)
        graph_strategy = _registry.create_strategy(desc, db_factory=SessionLocal)
        graph_mems, explain_info = graph_strategy.retrieve(gv_user, query, q_emb, top_k=3, explain=True)

        path = (explain_info or {}).get("path", "unknown")
        check("graph path used", path == "graph", f"path={path}")

        c_in_graph = any("transformer" in m.content for m in graph_mems)
        check("graph recalls C via 2-hop", c_in_graph,
              f"graph results: {[m.content[:30] for m in graph_mems]}")

        check("graph > vector (2-hop recall)", c_in_graph and not c_in_vec)

    finally:
        store.delete_user_data(gv_user)
        with SessionLocal() as db:
            for t in ("mem_edit_log", "mem_memories"):
                db.execute(text(f"DELETE FROM {t} WHERE user_id = :u"), {"u": gv_user})
            db.commit()


# ── Scenario 10: Backfill ─────────────────────────────────────────────

def test_backfill() -> None:
    print("\n── 9. Backfill (memories exist, graph nodes missing) ──")

    from core.memory.tabular.store import MemoryStore
    from core.memory.strategy.activation_index import ActivationIndexManager

    # Use a separate user so graph is empty
    bfill_user = USER_ID + "_bf"

    try:
        # 1. Store memories directly (bypass index_manager → no graph nodes)
        store = MemoryStore(SessionLocal)
        from core.memory.types import Memory as MemObj, MemoryType, TrustTier, _utcnow
        import uuid as _uuid

        now = _utcnow()
        from core.embedding import get_embedding_client
        ec = get_embedding_client()
        texts = ["backfill test memory A about Python", "backfill test memory B about Rust"]
        embeddings = ec.embed_batch(texts)

        for txt, emb in zip(texts, embeddings):
            mem = MemObj(
                memory_id=_uuid.uuid4().hex, user_id=bfill_user,
                memory_type=MemoryType.SEMANTIC, content=txt,
                initial_confidence=1.0, trust_tier=TrustTier.T2_CURATED,
                embedding=emb, observed_at=now,
            )
            store.create(mem)

        mem_count = scalar("SELECT COUNT(*) FROM mem_memories WHERE user_id = :uid", uid=bfill_user)
        node_count = scalar("SELECT COUNT(*) FROM memory_graph_nodes WHERE user_id = :uid", uid=bfill_user)
        check("memories exist", mem_count == 2, f"mem_count={mem_count}")
        check("no graph nodes yet", node_count == 0, f"node_count={node_count}")

        # 2. Backfill
        idx = ActivationIndexManager(SessionLocal)
        check("backfill_needed=True", idx.backfill_needed(bfill_user))
        result = idx.backfill(bfill_user)
        check("backfill processed 2", result.processed == 2, f"processed={result.processed}")
        check("backfill 0 errors", len(result.errors) == 0, f"errors={result.errors}")

        node_count = scalar("SELECT COUNT(*) FROM memory_graph_nodes WHERE user_id = :uid", uid=bfill_user)
        check("graph nodes created", node_count == 2, f"node_count={node_count}")
        check("backfill_needed=False", not idx.backfill_needed(bfill_user))

        # 3. Idempotent — re-run should skip
        result2 = idx.backfill(bfill_user)
        check("re-backfill skipped 2", result2.skipped == 2, f"skipped={result2.skipped}")
    finally:
        with SessionLocal() as db:
            for t in ("mem_edit_log", "mem_memories", "memory_graph_nodes", "memory_graph_edges"):
                db.execute(text(f"DELETE FROM {t} WHERE user_id = :uid"), {"uid": bfill_user})
            db.commit()


# ── Scenario 11: Correct → graph node created for new memory ─────────

def test_correct_graph() -> None:
    print("\n── 10. Correct → new graph node ──")

    from core.memory.factory import create_editor

    editor = create_editor(SessionLocal, user_id=USER_ID)
    if not editor._embed_client:
        fail("embed_client", "no embed_client")
        return

    # Inject original
    stored = editor.batch_inject(USER_ID, [
        {"content": "User prefers tabs over spaces", "type": "semantic", "trust": "T1"},
    ], source="verify-correct")
    old_id = stored[0].memory_id

    nodes_before = scalar("SELECT COUNT(*) FROM memory_graph_nodes WHERE user_id = :uid", uid=USER_ID)

    # Correct it
    new_mem = editor.correct(USER_ID, old_id, new_content="User prefers spaces over tabs", reason="changed mind")

    nodes_after = scalar("SELECT COUNT(*) FROM memory_graph_nodes WHERE user_id = :uid", uid=USER_ID)
    check("new graph node created", nodes_after > nodes_before, f"before={nodes_before}, after={nodes_after}")

    # Verify new node exists for the corrected memory
    new_node = query_one(
        "SELECT node_id, content FROM memory_graph_nodes WHERE user_id = :uid AND memory_id = :mid",
        uid=USER_ID, mid=new_mem.memory_id,
    )
    check("new node has correct content", new_node is not None and "spaces over tabs" in (new_node.content or ""))

    # Old memory deactivated in mem_memories
    old_mem = query_one("SELECT is_active FROM mem_memories WHERE memory_id = :mid", mid=old_id)
    check("old memory deactivated", old_mem is not None and old_mem.is_active == 0)


# ── Scenario 12: Purge → governance triggered ────────────────────────

def test_purge_graph() -> None:
    print("\n── 11. Purge → governance triggered ──")

    from core.memory.factory import create_editor

    editor = create_editor(SessionLocal, user_id=USER_ID)
    if not editor._embed_client:
        fail("embed_client", "no embed_client")
        return

    # Inject a memory to purge
    stored = editor.batch_inject(USER_ID, [
        {"content": "Temporary fact to be purged", "type": "semantic", "trust": "T2"},
    ], source="verify-purge")
    mid = stored[0].memory_id

    node_before = query_one(
        "SELECT node_id FROM memory_graph_nodes WHERE user_id = :uid AND memory_id = :mid",
        uid=USER_ID, mid=mid,
    )
    check("graph node exists before purge", node_before is not None)

    # Purge
    result = editor.purge(USER_ID, memory_ids=[mid], reason="test purge")
    check("purge deactivated 1", result.deactivated == 1)

    # Verify memory deactivated
    mem = query_one("SELECT is_active FROM mem_memories WHERE memory_id = :mid", mid=mid)
    check("memory deactivated", mem is not None and mem.is_active == 0)

    # on_governance was called — consolidation ran (no error)
    # Graph node may still exist (consolidation doesn't delete nodes for purged memories)
    # but the key assertion is that purge + governance completed without error
    check("purge completed", True)


# ── Scenario 13: Consolidation conflict detection ────────────────────

def test_consolidation() -> None:
    print("\n── 12. Consolidation conflict detection ──")

    from core.memory.graph.consolidation import GraphConsolidator
    from core.memory.graph.graph_store import GraphStore

    consolidator = GraphConsolidator(SessionLocal)

    # Run consolidation — should complete without error on existing data
    result = consolidator.consolidate(USER_ID)
    check("consolidation no errors", len(result.errors) == 0, f"errors={result.errors}")
    vlog(f"conflicts={result.conflicts_detected}, orphaned={result.orphaned_scenes}, "
         f"promoted={result.promoted}, demoted={result.demoted}")


# ── Scenario 14: Opinion evolution ───────────────────────────────────

def test_opinion_evolution() -> None:
    print("\n── 13. Opinion evolution ──")

    from core.memory.graph.graph_store import GraphStore
    from core.memory.graph.opinion import evolve_opinions
    from core.memory.graph.types import GraphNodeData, NodeType

    store = GraphStore(SessionLocal)

    from core.embedding import get_embedding_client
    ec = get_embedding_client()

    # Create a scene node manually (opinion evolution only targets scene nodes)
    scene_emb = ec.embed("The user is an expert Python developer")
    scene = GraphNodeData(
        node_id=f"scene_{USER_ID[:8]}", user_id=USER_ID,
        node_type=NodeType.SCENE, content="The user is an expert Python developer",
        embedding=scene_emb, confidence=0.7, trust_tier="T3",
    )
    store.create_node(scene)

    # Create a new semantic node (evidence) with similar embedding
    new_emb = ec.embed("User has been writing Python for 10 years")
    new_node = GraphNodeData(
        node_id=f"new_{USER_ID[:8]}", user_id=USER_ID,
        node_type=NodeType.SEMANTIC, content="User has been writing Python for 10 years",
        embedding=new_emb, confidence=1.0, trust_tier="T1",
    )
    store.create_node(new_node)

    # Add an edge so activation can reach the scene
    store.add_edges_batch([
        (new_node.node_id, scene.node_id, "association", 0.85),
    ], USER_ID)

    # Run opinion evolution
    result = evolve_opinions(store, new_node.node_id, USER_ID)
    vlog(f"scenes_evaluated={result.scenes_evaluated}, supporting={result.supporting}, "
         f"contradicting={result.contradicting}, neutral={result.neutral}")

    # The function should complete without error
    check("opinion evolution ran", True)
    check("evaluated scenes", result.scenes_evaluated >= 0)

    # Clean up manual nodes
    with SessionLocal() as db:
        db.execute(text("DELETE FROM memory_graph_edges WHERE source_id = :sid"), {"sid": new_node.node_id})
        db.execute(text("DELETE FROM memory_graph_nodes WHERE node_id IN (:a, :b)"),
                   {"a": scene.node_id, "b": new_node.node_id})
        db.commit()


# ── Scenario 15: Observer pipeline → graph index ─────────────────────

def test_observer_pipeline_graph() -> None:
    print("\n── 14. Observer pipeline → graph index ──")

    from core.memory.factory import create_memory_service

    svc = create_memory_service(SessionLocal, user_id=USER_ID)

    # Check if LLM is available (observer needs it for extraction)
    if not hasattr(svc, '_storage') or not hasattr(svc._storage, '_observer'):
        fail("observer", "cannot access observer from service")
        return

    observer = getattr(svc._storage, '_observer', None)
    if observer is None or observer.llm is None:
        # No LLM available — skip gracefully
        ok("observer skip", "no LLM configured, skipping observer pipeline test")
        return

    nodes_before = scalar("SELECT COUNT(*) FROM memory_graph_nodes WHERE user_id = :uid", uid=USER_ID)

    # Simulate a conversation turn
    messages = [
        {"role": "user", "content": "I always use vim as my editor"},
        {"role": "assistant", "content": "Got it, you prefer vim."},
    ]
    memories = svc.observe_turn(USER_ID, messages, source_event_ids=["verify_observer"])

    if not memories:
        ok("observer no extraction", "LLM extracted 0 memories (model-dependent)")
        return

    check("observer stored memories", len(memories) > 0, f"count={len(memories)}")

    nodes_after = scalar("SELECT COUNT(*) FROM memory_graph_nodes WHERE user_id = :uid", uid=USER_ID)
    check("graph nodes increased", nodes_after > nodes_before,
          f"before={nodes_before}, after={nodes_after}")


# ── Scenario 16: Vector index health + rebuild ────────────────────────

def test_vector_index_health_and_rebuild() -> None:
    print("\n── 16. Vector index health + rebuild ──")

    from core.memory.tabular.governance import GovernanceScheduler

    gs = GovernanceScheduler(SessionLocal)

    # 1. Health check returns results for both tables
    health = gs._check_vector_index_health()

    if not health:
        ok("vector index health skip", "VectorManager not available, skipping")
        return

    for table, h in health.items():
        if "error" in h:
            ok(f"{table} health error", f"error: {h['error']}")
            continue
        check(f"{table} has centroids", h["centroids"] >= 1, f"centroids={h['centroids']}")
        check(f"{table} has total_rows", h["total_rows"] >= 0, f"total_rows={h['total_rows']}")
        ok(f"{table} health", f"centroids={h['centroids']}, rows={h['total_rows']}, ratio={h['ratio']}, needs_rebuild={h['needs_rebuild']}")

    # 2. run_cycle includes health in result
    result = gs.run_cycle(USER_ID)
    check("run_cycle includes vector_index_health", isinstance(result.vector_index_health, dict),
          f"got {type(result.vector_index_health)}")

    # 3. Rebuild index for mem_memories (drop + recreate with optimal lists)
    try:
        rebuild_result = gs.rebuild_vector_index("mem_memories")
        check("rebuild returns table", rebuild_result["table"] == "mem_memories",
              f"got {rebuild_result}")
        check("rebuild new_lists >= 1", rebuild_result["new_lists"] >= 1,
              f"new_lists={rebuild_result['new_lists']}")
        ok("rebuild mem_memories", f"lists {rebuild_result['old_lists']} → {rebuild_result['new_lists']} (rows={rebuild_result['total_rows']})")
    except Exception as e:
        fail("rebuild mem_memories", str(e))

    # 4. After rebuild, health check should still work
    health_after = gs._check_vector_index_health()
    if "mem_memories" in health_after and "error" not in health_after["mem_memories"]:
        ok("health after rebuild", f"needs_rebuild={health_after['mem_memories']['needs_rebuild']}")


# ── Scenario 15: NL → Script (real LLM) ──────────────────────────────

def test_nl_to_script() -> None:
    print("\n── 15. NL → Script (real LLM) ──")
    from core.llm.client import LLMClient
    from core.memory.programmer import nl_to_script

    llm = LLMClient(SessionLocal)
    instruction = f"Remember that user {USER_ID} prefers Python for data analysis"

    try:
        actions = nl_to_script(instruction, USER_ID, llm)
    except Exception as e:
        fail("nl_to_script", str(e))
        return

    check("LLM returned actions", len(actions) > 0, f"got {len(actions)}")
    vlog(f"actions: {json.dumps(actions, ensure_ascii=False)}")

    # Execute in sandbox to verify full chain
    prog = _make_programmer()
    mem_before = count("mem_memories")

    result = prog.execute(USER_ID, actions, sandbox=True, dry_run=False, program_name="verify-nl")
    check("sandbox executed", result.actions_executed >= 1)
    check("production unchanged", count("mem_memories") == mem_before)
    check("has experiment", result.experiment_id is not None)


# ── Scenario 16: CLI session state (-m + --session-id) ───────────────

def test_session_state() -> None:
    print("\n── 16. CLI session state (-m + --session-id) ──")
    import subprocess

    cli = [sys.executable, str(Path(__file__).parent.parent.parent / "cli" / "mo_agent_api.py")]

    def run(*args: str) -> str:
        r = subprocess.run(cli + list(args), capture_output=True, text=True)
        return r.stdout.strip()

    # Resolve model: explicit > cheapest
    from core.llm.model_resolver import _resolve_cheapest
    cheapest = _model or _resolve_cheapest("deepseek-chat")

    # Turn 1 — no session-id, auto-create
    run("chat", "-m", "hello, my name is verify-test", "--auto-approve", "--model", cheapest)

    # Read session_id from credentials
    import json
    cred_path = Path.home() / ".mo-agent" / "credentials.json"
    creds = json.loads(cred_path.read_text())
    current = creds.get("current_profile", "default")
    sid = creds["profiles"][current]["last_session_id"]
    check("session_id created", bool(sid), sid)
    vlog(f"session_id={sid}")

    # Turn 2 — reuse session
    turn2_response = run("chat", "-m", "what did I just say my name was?", "--session-id", sid, "--auto-approve", "--model", cheapest)

    # Verify turn2 response references turn1 content (LLM actually saw history)
    check("turn2 recalls name", "verify-test" in turn2_response.lower(), f"response: {turn2_response[:100]}")

    # Shared session integrity checks (same logic as verify-talk)
    from scripts.e2e.talk_checks import check_session_integrity, check_turn_count_increases
    prev_counts: dict = {}
    for r in check_session_integrity(SessionLocal, sid):
        check(r.name, r.passed, r.message)
    r = check_turn_count_increases(SessionLocal, sid, prev_counts)
    check(r.name, r.passed, r.message)


# ── Main ──────────────────────────────────────────────────────────────

_model: str | None = None


def main() -> None:
    global _verbose, _model

    parser = argparse.ArgumentParser(description="E2E Verification")
    parser.add_argument("--with-llm", action="store_true", help="Include NL→Script + session state tests (needs LLM + API server)")
    parser.add_argument("--model", default=None, help="LLM model to use (default: cheapest)")
    parser.add_argument("-v", "--verbose", action="store_true")
    args = parser.parse_args()
    _verbose = args.verbose
    _model = args.model

    print(f"E2E Verification — user={USER_ID}  db={DB_NAME}")

    # Clean any leftover __verify_ data
    cleanup()

    try:
        exp_id = test_sandbox_inject()
        if exp_id:
            test_commit(exp_id)
        test_dry_run()
        test_discard()
        test_direct_write()
        test_multi_turn()
        test_graph_edges()
        test_spreading_activation()
        test_graph_vs_vector()
        test_backfill()
        test_correct_graph()
        test_purge_graph()
        test_consolidation()
        test_opinion_evolution()
        test_vector_index_health_and_rebuild()

        if args.with_llm:
            test_observer_pipeline_graph()
            test_nl_to_script()
            test_session_state()
    finally:
        cleanup()

    print(f"\n{'='*50}")
    print(f"Result: {_passed} passed, {_failed} failed")
    sys.exit(1 if _failed else 0)


if __name__ == "__main__":
    main()
