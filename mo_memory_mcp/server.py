"""mo-memory MCP server.

Exposes memory_store, memory_retrieve, memory_correct, memory_purge,
memory_profile, memory_search tools via MCP protocol.

Two backends:
  - EmbeddedBackend: direct DB access (local / stdio mode)
  - HTTPBackend: proxies to memory service REST API (remote mode)
"""

from __future__ import annotations

import argparse
import asyncio
import logging
import os
from typing import Any

from mcp.server import FastMCP

logger = logging.getLogger(__name__)

# ── Backend protocol ──────────────────────────────────────────────────


class MemoryBackend:
    """Abstract backend for memory operations."""

    def store(self, user_id: str, content: str, memory_type: str, session_id: str | None) -> dict: ...
    def retrieve(self, user_id: str, query: str, top_k: int, session_id: str | None = None) -> list[dict]: ...
    def correct(self, user_id: str, memory_id: str, new_content: str, reason: str) -> dict: ...
    def purge(self, user_id: str, memory_id: str | None, topic: str | None, reason: str) -> dict: ...
    def profile(self, user_id: str) -> dict: ...
    def search(self, user_id: str, query: str, top_k: int) -> list[dict]: ...
    def governance(self, user_id: str, force: bool = False) -> dict: ...
    def consolidate(self, user_id: str, force: bool = False) -> dict: ...
    def reflect(self, user_id: str, force: bool = False) -> dict: ...
    def rebuild_index(self, table: str) -> str: ...
    def health_warnings(self, user_id: str) -> list[str]: ...
    # Branching
    def snapshot_create(self, user_id: str, name: str, description: str) -> dict: ...
    def snapshot_list(self, user_id: str) -> list[dict]: ...
    def snapshot_rollback(self, user_id: str, name: str) -> dict: ...
    def branch_create(self, user_id: str, name: str, from_snapshot: str | None, from_timestamp: str | None) -> dict: ...
    def branch_list(self, user_id: str) -> list[dict]: ...
    def branch_checkout(self, user_id: str, name: str) -> dict: ...
    def branch_delete(self, user_id: str, name: str) -> dict: ...
    def branch_merge(self, user_id: str, source: str, strategy: str) -> dict: ...


class EmbeddedBackend(MemoryBackend):
    """Direct DB access — for local stdio mode."""

    def __init__(self, db_url: str | None = None) -> None:
        import sys

        # MatrixOne SDK adds a StreamHandler(stdout) during Client().
        # Temporarily redirect stdout → stderr to prevent protocol pollution.
        _real_stdout = sys.stdout
        sys.stdout = sys.stderr
        try:
            if db_url:
                from sqlalchemy import create_engine
                from sqlalchemy.orm import sessionmaker
                engine = create_engine(db_url, pool_pre_ping=True)
                self._db_factory = sessionmaker(bind=engine)
            else:
                from api.database import SessionLocal
                self._db_factory = SessionLocal
            from core.memory.factory import create_editor, create_memory_service
        finally:
            sys.stdout = _real_stdout

        # Replace matrixone's stdout handler with stderr permanently.
        _mo = logging.getLogger("matrixone")
        _mo.handlers = [logging.StreamHandler(sys.stderr)]
        _mo.setLevel(logging.WARNING)

        # Configure embedding from env vars (standalone) or project settings (dev).
        self._embed_client = self._make_embed_client() if db_url else None
        self._create_service = create_memory_service
        self._create_editor = create_editor

    @staticmethod
    def _make_embed_client():
        """Build EmbeddingClient from MO_MEMORY_EMBEDDING_* env vars."""
        provider = os.environ.get("MO_MEMORY_EMBEDDING_PROVIDER", "local")
        model = os.environ.get("MO_MEMORY_EMBEDDING_MODEL", "all-MiniLM-L6-v2")
        dim = int(os.environ.get("MO_MEMORY_EMBEDDING_DIM", "384"))
        try:
            from core.embedding.client import EmbeddingClient
            return EmbeddingClient(
                provider=provider, model=model, dim=dim,
                api_key=os.environ.get("MO_MEMORY_EMBEDDING_API_KEY", ""),
                base_url=os.environ.get("MO_MEMORY_EMBEDDING_BASE_URL"),
            )
        except Exception:
            logger.warning("Embedding client not available, memories won't be vectorized")
            return None

    def store(self, user_id: str, content: str, memory_type: str, session_id: str | None) -> dict:
        from core.memory.types import MemoryType

        editor = self._create_editor(self._db_factory, user_id=user_id, embed_client=self._embed_client)
        mem = editor.inject(user_id, content, memory_type=MemoryType(memory_type), source="mcp", session_id=session_id)
        return {"memory_id": mem.memory_id, "content": mem.content}

    def retrieve(self, user_id: str, query: str, top_k: int, session_id: str | None = None) -> list[dict]:
        svc = self._create_service(self._db_factory, user_id=user_id)
        # Pass session_id to the underlying retrieval service.
        # The service's retrieve() method accepts session_id (default "") and uses it to prioritize
        # memories from that session. If session_id is None, it's converted to "" by the service,
        # which enables cross-session retrieval with include_cross_session=True.
        memories, _ = svc.retrieve(user_id, query, top_k=top_k, session_id=session_id or "")
        return [{"memory_id": m.memory_id, "content": m.content, "type": str(m.memory_type)} for m in memories]

    # Thresholds for health_warnings — surfaced as constants for testability.
    _LOW_CONFIDENCE_THRESHOLD = 0.4
    _LOW_CONFIDENCE_WARNING_MIN = 5

    def health_warnings(self, user_id: str) -> list[str]:
        """Lightweight check for memory quality issues."""
        warnings: list[str] = []
        try:
            from sqlalchemy import text
            with self._db_factory() as db:
                row = db.execute(text(
                    "SELECT COUNT(*) as cnt FROM mem_memories "
                    "WHERE user_id = :uid AND is_active = 1 "
                    "AND initial_confidence < :threshold"
                ), {"uid": user_id, "threshold": self._LOW_CONFIDENCE_THRESHOLD}).fetchone()
                if row and row.cnt >= self._LOW_CONFIDENCE_WARNING_MIN:
                    warnings.append(f"{row.cnt} memories have low confidence — consider reviewing with memory_search.")
        except Exception as e:
            logger.debug("health_warnings query failed for user=%s: %s", user_id, e)
        return warnings

    def correct(self, user_id: str, memory_id: str, new_content: str, reason: str) -> dict:
        editor = self._create_editor(self._db_factory, user_id=user_id, embed_client=self._embed_client)
        mem = editor.correct(user_id, memory_id, new_content, reason=reason)
        return {"memory_id": mem.memory_id, "content": mem.content}

    def purge(self, user_id: str, memory_id: str | None, topic: str | None, reason: str) -> dict:
        editor = self._create_editor(self._db_factory, user_id=user_id, embed_client=self._embed_client)
        if topic:
            # Use SQL LIKE for precise keyword matching.  Semantic search
            # (self.retrieve) would return loosely related results ranked by
            # similarity with no score threshold — too dangerous for a
            # destructive bulk operation.
            from sqlalchemy import text
            with self._db_factory() as db:
                rows = db.execute(text(
                    "SELECT memory_id FROM mem_memories "
                    "WHERE user_id = :uid AND is_active = 1 "
                    "AND content LIKE :pattern"
                ), {"uid": user_id, "pattern": f"%{topic}%"}).fetchall()
            ids = [r.memory_id for r in rows]
            if not ids:
                return {"purged": 0}
            result = editor.purge(user_id, memory_ids=ids, reason=reason or f"topic purge: {topic}")
        elif memory_id:
            result = editor.purge(user_id, memory_ids=[memory_id], reason=reason)
        else:
            return {"purged": 0}
        return {"purged": result.deactivated}

    def profile(self, user_id: str) -> dict:
        svc = self._create_service(self._db_factory, user_id=user_id)
        return {"user_id": user_id, "profile": svc.get_profile(user_id)}

    def search(self, user_id: str, query: str, top_k: int) -> list[dict]:
        return self.retrieve(user_id, query, top_k)

    # Cooldown: governance/consolidate/reflect are expensive, throttle per user.
    # key = (user_id, op_name), value = (timestamp, result)
    _cooldown_cache: dict[tuple[str, str], tuple[float, dict]] = {}
    _COOLDOWN_SECONDS = {"governance": 3600, "consolidate": 1800, "reflect": 7200}

    def _with_cooldown(self, user_id: str, op: str, fn: Any, force: bool = False) -> dict:
        import time
        key = (user_id, op)
        now = time.time()
        if not force:
            cached = self._cooldown_cache.get(key)
            if cached:
                ts, result = cached
                remaining = self._COOLDOWN_SECONDS[op] - (now - ts)
                if remaining > 0:
                    result_copy = dict(result)
                    result_copy["skipped"] = True
                    result_copy["cooldown_remaining_s"] = int(remaining)
                    return result_copy
        result = fn()
        self._cooldown_cache[key] = (now, result)
        return result

    def governance(self, user_id: str, force: bool = False) -> dict:
        def _run():
            from core.memory.tabular.governance import GovernanceScheduler
            gs = GovernanceScheduler(self._db_factory)
            result = gs.run_cycle(user_id)
            return {
                "quarantined": result.quarantined,
                "cleaned_stale": result.cleaned_stale,
                "scenes_created": result.scenes_created,
                "vector_index_health": result.vector_index_health,
            }
        return self._with_cooldown(user_id, "governance", _run, force=force)

    def consolidate(self, user_id: str, force: bool = False) -> dict:
        def _run():
            from core.memory.graph.consolidation import GraphConsolidator
            gc = GraphConsolidator(self._db_factory)
            result = gc.consolidate(user_id)
            return {
                "merged_nodes": result.merged_nodes,
                "conflicts_detected": result.conflicts_detected,
                "orphaned_scenes": result.orphaned_scenes,
                "promoted": result.promoted,
                "demoted": result.demoted,
            }
        return self._with_cooldown(user_id, "consolidate", _run, force=force)

    def reflect(self, user_id: str, force: bool = False) -> dict:
        def _run():
            from core.memory.graph.candidates import GraphCandidateProvider
            from core.memory.reflection.engine import ReflectionEngine
            from core.memory.graph.service import GraphMemoryService

            provider = GraphCandidateProvider(self._db_factory)
            svc = GraphMemoryService(self._db_factory)
            try:
                from core.llm.client import LLMClient
                llm = LLMClient(db_factory=self._db_factory)
            except Exception:
                return {"error": "LLM client not available for reflection"}
            engine = ReflectionEngine(provider, svc, llm)
            result = engine.reflect(user_id)
            return {"scenes_created": result.scenes_created, "candidates_found": result.candidates_found}
        return self._with_cooldown(user_id, "reflect", _run, force=force)

    def rebuild_index(self, table: str) -> str:
        from core.memory.tabular.governance import GovernanceScheduler
        gs = GovernanceScheduler(self._db_factory)
        result = gs.rebuild_vector_index(table)
        return f"Rebuilt IVF index for {table}: lists {result['old_lists']} → {result['new_lists']} (rows={result['total_rows']})"

    # ── Branching ─────────────────────────────────────────────────────

    MAX_USER_SNAPSHOTS = 1000
    MAX_USER_BRANCHES = 20
    _BRANCH_TABLES = ("mem_memories", "memory_graph_nodes", "memory_graph_edges")

    @staticmethod
    def _sanitize_name(name: str) -> str:
        import re
        clean = re.sub(r"[^a-zA-Z0-9_]", "_", name)[:40]
        if not clean or not clean[0].isalpha():
            clean = "s_" + clean
        return clean

    def _git(self):
        from core.git_for_data import GitForData
        return GitForData(self._db_factory)

    def _source_db_name(self) -> str:
        from api.database import SessionLocal
        return SessionLocal.kw["bind"].url.database

    # In-memory active branch tracking per user (session-scoped, not persisted across restarts)
    _active_branches: dict[str, str] = {}

    def _get_active_branch(self, user_id: str) -> str:
        """Get active branch for user. Stored in-memory for this session."""
        return self._active_branches.get(user_id, "main")

    def _set_active_branch(self, user_id: str, name: str) -> None:
        """Set active branch for user. Stored in-memory for this session."""
        self._active_branches[user_id] = name

    def snapshot_create(self, user_id: str, name: str, description: str) -> dict:
        safe = self._sanitize_name(name)
        from sqlalchemy import text
        with self._db_factory() as db:
            cnt = db.execute(text("SELECT COUNT(*) FROM mo_catalog.mo_snapshots")).scalar() or 0
        if cnt >= self.MAX_USER_SNAPSHOTS:
            return {"error": f"Snapshot limit reached ({self.MAX_USER_SNAPSHOTS}). Delete old snapshots first."}
        snap_name = f"mem_snap_{safe}"
        info = self._git().create_snapshot(snap_name)
        return {"name": name, "snapshot_name": snap_name, "timestamp": str(info.get("timestamp", ""))}

    def snapshot_list(self, user_id: str) -> list[dict]:
        all_snaps = self._git().list_snapshots()
        result = []
        for s in all_snaps:
            sname = s["snapshot_name"]
            if sname.startswith("mem_snap_") or sname.startswith("mem_milestone_"):
                display = sname.replace("mem_snap_", "").replace("mem_milestone_", "auto:")
                result.append({"name": display, "snapshot_name": sname, "timestamp": str(s.get("timestamp", ""))})
        return sorted(result, key=lambda x: x["timestamp"], reverse=True)

    def snapshot_rollback(self, user_id: str, name: str) -> dict:
        safe = self._sanitize_name(name)
        snap_name = name if name.startswith("mem_snap_") or name.startswith("mem_milestone_") else f"mem_snap_{safe}"
        git = self._git()
        for table in ("mem_memories", "memory_graph_nodes", "memory_graph_edges", "mem_edit_log"):
            try:
                git.restore_table_from_snapshot(table, snap_name)
            except Exception as e:
                if table == "mem_memories":
                    return {"error": f"Rollback failed: {e}"}
                logger.debug("Rollback table %s skipped: %s", table, e)
        return {"rolled_back_to": snap_name}

    def branch_create(self, user_id: str, name: str, from_snapshot: str | None, from_timestamp: str | None = None) -> dict:
        if from_snapshot and from_timestamp:
            return {"error": "Specify from_snapshot or from_timestamp, not both."}
        safe = self._sanitize_name(name)
        from sqlalchemy import text

        # Global branch limit (not per-user). Prevents resource exhaustion across all users.
        with self._db_factory() as db:
            active = db.execute(text("SELECT COUNT(*) FROM mem_branches WHERE status = 'active'")).scalar() or 0
        if active >= self.MAX_USER_BRANCHES:
            return {"error": f"Branch limit reached ({self.MAX_USER_BRANCHES}). Delete old branches first."}

        # Duplicate check: reject if branch with same name already exists (active or deleted).
        # This prevents name reuse confusion. Deleted branches are soft-deleted and can be purged later.
        with self._db_factory() as db:
            dup = db.execute(text(
                "SELECT branch_id FROM mem_branches WHERE user_id = :uid AND name = :name AND status != 'purged'"
            ), {"uid": user_id, "name": safe}).fetchone()
        if dup:
            return {"error": f"Branch '{safe}' already exists or was recently deleted. Use a different name."}

        snap = from_snapshot
        if snap and not snap.startswith("mem_snap_") and not snap.startswith("mem_milestone_"):
            snap = f"mem_snap_{self._sanitize_name(snap)}"

        # Validate timestamp: within last 30 minutes
        if from_timestamp:
            from datetime import datetime, timezone, timedelta
            try:
                ts = datetime.strptime(from_timestamp, "%Y-%m-%d %H:%M:%S").replace(tzinfo=timezone.utc)
            except ValueError:
                return {"error": "from_timestamp must be 'YYYY-MM-DD HH:MM:SS'"}
            now = datetime.now(timezone.utc)
            if ts > now:
                return {"error": "from_timestamp cannot be in the future"}
            if now - ts > timedelta(minutes=30):
                return {"error": "from_timestamp must be within the last 30 minutes"}

        from core.utils.id_generator import generate_id
        branch_id = generate_id()
        branch_db = f"mem_br_{branch_id}"
        src_db = self._source_db_name()

        # Determine branch point
        snap_name: str | None = None
        if snap:
            snap_name = snap
            try:
                self._git().create_snapshot(snap_name)
            except Exception:
                pass
        elif not from_timestamp:
            snap_name = f"mem_br_base_{branch_id}"
            self._git().create_snapshot(snap_name)

        # CREATE DATABASE (DDL, separate commit)
        with self._db_factory() as db:
            db.commit()
            db.execute(text(f"DROP DATABASE IF EXISTS `{branch_db}`"))
            db.commit()
            db.execute(text(f"CREATE DATABASE `{branch_db}`"))
            db.commit()

        try:
            # Branch tables + INSERT mem_branches in one commit
            from matrixone.branch_builder import create_table_branch
            with self._db_factory() as db:
                for table in self._BRANCH_TABLES:
                    try:
                        if snap_name:
                            stmt = create_table_branch(f"{branch_db}.{table}").from_table(
                                f"{src_db}.{table}", snapshot=snap_name
                            )
                            db.execute(text(str(stmt)))
                        else:
                            # timestamp mode — SDK doesn't support yet
                            db.execute(text(
                                f"data branch create table {branch_db}.{table} "
                                f'from {src_db}.{table}{{timestamp="{from_timestamp}"}}'
                            ))
                    except Exception as e:
                        if table == "mem_memories":
                            raise
                        logger.debug("Branch table %s failed: %s", table, e)
                base_label = snap_name or from_timestamp or "current"
                db.execute(text(
                    "INSERT INTO mem_branches (branch_id, user_id, name, branch_db, base_snapshot, status, created_at) "
                    "VALUES (:bid, :uid, :name, :bdb, :snap, 'active', NOW())"
                ), {"bid": branch_id, "uid": user_id, "name": safe, "bdb": branch_db, "snap": base_label})
                db.commit()
        except Exception:
            with self._db_factory() as db:
                db.commit()
                db.execute(text(f"DROP DATABASE IF EXISTS `{branch_db}`"))
                db.commit()
            raise

        return {"name": safe, "branch_db": branch_db, "branch_id": branch_id}

    def branch_list(self, user_id: str) -> list[dict]:
        from sqlalchemy import text
        with self._db_factory() as db:
            rows = db.execute(text(
                "SELECT branch_id, name, branch_db, created_at "
                "FROM mem_branches WHERE user_id = :uid AND status = 'active' "
                "ORDER BY created_at"
            ), {"uid": user_id}).fetchall()
        active = self._get_active_branch(user_id)  # Call once, not per row
        result = [{"name": "main", "branch_db": self._source_db_name(), "active": active == "main"}]
        for r in rows:
            result.append({"name": r.name, "branch_db": r.branch_db, "active": active == r.name})
        return result

    def branch_checkout(self, user_id: str, name: str) -> dict:
        if name == "main":
            self._set_active_branch(user_id, "main")
            return {"active_branch": "main"}
        from sqlalchemy import text
        with self._db_factory() as db:
            row = db.execute(text(
                "SELECT name FROM mem_branches WHERE user_id = :uid AND name = :name AND status = 'active'"
            ), {"uid": user_id, "name": name}).fetchone()
        if not row:
            return {"error": f"Branch '{name}' not found"}
        self._set_active_branch(user_id, name)
        return {"active_branch": name}

    def branch_delete(self, user_id: str, name: str) -> dict:
        if name == "main":
            return {"error": "Cannot delete main"}
        from sqlalchemy import text
        with self._db_factory() as db:
            row = db.execute(text(
                "SELECT branch_id, branch_db FROM mem_branches "
                "WHERE user_id = :uid AND name = :name AND status = 'active'"
            ), {"uid": user_id, "name": name}).fetchone()
        if not row:
            return {"error": f"Branch '{name}' not found"}

        # delete_table_branch + mark deleted in one commit
        try:
            from matrixone.branch_builder import delete_table_branch
            with self._db_factory() as db:
                for table in self._BRANCH_TABLES:
                    try:
                        stmt = delete_table_branch(f"{row.branch_db}.{table}")
                        db.execute(text(str(stmt)))
                    except Exception:
                        pass
                db.execute(text(
                    "UPDATE mem_branches SET status = 'deleted', updated_at = NOW() WHERE branch_id = :bid"
                ), {"bid": row.branch_id})
                db.commit()
        except Exception:
            logger.warning("Failed to delete branch tables %s", row.branch_db)

        # DROP DATABASE is DDL, must be separate
        try:
            with self._db_factory() as db:
                db.commit()
                db.execute(text(f"DROP DATABASE IF EXISTS `{row.branch_db}`"))
                db.commit()
        except Exception:
            logger.warning("Failed to drop branch DB %s", row.branch_db)

        if self._get_active_branch(user_id) == name:
            self._set_active_branch(user_id, "main")
        return {"deleted": name}

    def branch_merge(self, user_id: str, source: str, strategy: str) -> dict:
        """Merge branch memories into current branch (usually main).
        
        Conflict detection: two memories conflict when they have the same memory_type
        and high semantic similarity (cosine > 0.9) but different content.
        
        Strategy:
        - 'append' (default): Add all branch memories, skip conflicts
        - 'replace': Branch memories override conflicting main memories
        
        Optimized for massive datasets (100M+): SQL-level conflict detection.
        """
        from sqlalchemy import text
        from core.utils.id_generator import generate_id
        
        with self._db_factory() as db:
            row = db.execute(text(
                "SELECT branch_id, branch_db FROM mem_branches "
                "WHERE user_id = :uid AND name = :name AND status = 'active'"
            ), {"uid": user_id, "name": source}).fetchone()
        if not row:
            return {"error": f"Branch '{source}' not found"}
        branch_db = row.branch_db

        with self._db_factory() as db:
            if strategy == "replace":
                # Replace strategy: update existing conflicts, insert new ones
                # 1. Update conflicts: same memory_type + cosine > 0.9 + different content
                db.execute(text(f"""
                    UPDATE mem_memories m
                    SET m.content = (
                        SELECT b.content FROM `{branch_db}`.mem_memories b
                        WHERE b.user_id = m.user_id
                        AND b.memory_type = m.memory_type
                        AND cosine_similarity(m.embedding, b.embedding) > 0.9
                        AND b.content != m.content
                        AND b.is_active = 1
                        LIMIT 1
                    ),
                    m.embedding = (
                        SELECT b.embedding FROM `{branch_db}`.mem_memories b
                        WHERE b.user_id = m.user_id
                        AND b.memory_type = m.memory_type
                        AND cosine_similarity(m.embedding, b.embedding) > 0.9
                        AND b.content != m.content
                        AND b.is_active = 1
                        LIMIT 1
                    ),
                    m.updated_at = NOW()
                    WHERE m.user_id = :uid
                    AND m.is_active = 1
                    AND EXISTS (
                        SELECT 1 FROM `{branch_db}`.mem_memories b
                        WHERE b.user_id = m.user_id
                        AND b.memory_type = m.memory_type
                        AND cosine_similarity(m.embedding, b.embedding) > 0.9
                        AND b.content != m.content
                        AND b.is_active = 1
                    )
                """), {"uid": user_id})
                
                # 2. Count updates
                updated = db.execute(text(f"""
                    SELECT COUNT(*) FROM `{branch_db}`.mem_memories b
                    WHERE b.user_id = :uid
                    AND b.is_active = 1
                    AND EXISTS (
                        SELECT 1 FROM mem_memories m
                        WHERE m.user_id = b.user_id
                        AND m.memory_type = b.memory_type
                        AND cosine_similarity(m.embedding, b.embedding) > 0.9
                        AND m.content != b.content
                        AND m.is_active = 1
                    )
                """), {"uid": user_id}).scalar() or 0
                
                # 3. Insert non-conflicting memories with new IDs
                db.execute(text(f"""
                    INSERT INTO mem_memories (memory_id, user_id, content, memory_type, 
                        initial_confidence, trust_tier, embedding, source_event_ids, 
                        is_active, observed_at, created_at, updated_at)
                    SELECT 
                        UUID(), b.user_id, b.content, b.memory_type,
                        b.initial_confidence, b.trust_tier, b.embedding, b.source_event_ids,
                        1, b.observed_at, NOW(), NOW()
                    FROM `{branch_db}`.mem_memories b
                    WHERE b.user_id = :uid
                    AND b.is_active = 1
                    AND NOT EXISTS (
                        SELECT 1 FROM mem_memories m
                        WHERE m.user_id = b.user_id
                        AND m.memory_type = b.memory_type
                        AND cosine_similarity(m.embedding, b.embedding) > 0.9
                        AND m.content != b.content
                        AND m.is_active = 1
                    )
                """), {"uid": user_id})
                
                # 4. Count inserts
                inserted = db.execute(text(f"""
                    SELECT COUNT(*) FROM `{branch_db}`.mem_memories b
                    WHERE b.user_id = :uid
                    AND b.is_active = 1
                    AND NOT EXISTS (
                        SELECT 1 FROM mem_memories m
                        WHERE m.user_id = b.user_id
                        AND m.memory_type = b.memory_type
                        AND cosine_similarity(m.embedding, b.embedding) > 0.9
                        AND m.content != b.content
                        AND m.is_active = 1
                    )
                """), {"uid": user_id}).scalar() or 0
                
                db.commit()
                merged = updated + inserted
                skipped = 0
            else:
                # Append strategy: insert only non-conflicting memories with new IDs
                db.execute(text(f"""
                    INSERT INTO mem_memories (memory_id, user_id, content, memory_type, 
                        initial_confidence, trust_tier, embedding, source_event_ids, 
                        is_active, observed_at, created_at, updated_at)
                    SELECT 
                        UUID(), b.user_id, b.content, b.memory_type,
                        b.initial_confidence, b.trust_tier, b.embedding, b.source_event_ids,
                        1, b.observed_at, NOW(), NOW()
                    FROM `{branch_db}`.mem_memories b
                    WHERE b.user_id = :uid
                    AND b.is_active = 1
                    AND NOT EXISTS (
                        SELECT 1 FROM mem_memories m
                        WHERE m.user_id = b.user_id
                        AND m.memory_type = b.memory_type
                        AND cosine_similarity(m.embedding, b.embedding) > 0.9
                        AND m.content != b.content
                        AND m.is_active = 1
                    )
                """), {"uid": user_id})
                
                merged = db.execute(text(f"""
                    SELECT COUNT(*) FROM `{branch_db}`.mem_memories b
                    WHERE b.user_id = :uid
                    AND b.is_active = 1
                    AND NOT EXISTS (
                        SELECT 1 FROM mem_memories m
                        WHERE m.user_id = b.user_id
                        AND m.memory_type = b.memory_type
                        AND cosine_similarity(m.embedding, b.embedding) > 0.9
                        AND m.content != b.content
                        AND m.is_active = 1
                    )
                """), {"uid": user_id}).scalar() or 0
                
                skipped = db.execute(text(f"""
                    SELECT COUNT(*) FROM `{branch_db}`.mem_memories b
                    WHERE b.user_id = :uid
                    AND b.is_active = 1
                    AND EXISTS (
                        SELECT 1 FROM mem_memories m
                        WHERE m.user_id = b.user_id
                        AND m.memory_type = b.memory_type
                        AND cosine_similarity(m.embedding, b.embedding) > 0.9
                        AND m.content != b.content
                        AND m.is_active = 1
                    )
                """), {"uid": user_id}).scalar() or 0
                
                db.commit()

        return {"merged": merged, "skipped": skipped, "source": source}

class HTTPBackend(MemoryBackend):
    """Proxy to memory service REST API — for remote mode."""

    def __init__(self, api_url: str, token: str | None = None) -> None:
        import httpx
        headers = {"Authorization": f"Bearer {token}"} if token else {}
        self._client = httpx.Client(base_url=api_url.rstrip("/"), headers=headers, timeout=30)

    def store(self, user_id: str, content: str, memory_type: str, session_id: str | None) -> dict:
        r = self._client.post("/v1/memories", json={"content": content, "memory_type": memory_type, "session_id": session_id})
        r.raise_for_status()
        return r.json()

    def retrieve(self, user_id: str, query: str, top_k: int, session_id: str | None = None) -> list[dict]:
        payload: dict[str, Any] = {"query": query, "top_k": top_k}
        # Only include session_id in payload if provided (not None).
        # This allows the remote API to distinguish between "no session context" (None)
        # and "empty session context" (""), enabling proper cross-session retrieval behavior.
        if session_id:
            payload["session_id"] = session_id
        r = self._client.post("/v1/memories/retrieve", json=payload)
        r.raise_for_status()
        return r.json()

    def correct(self, user_id: str, memory_id: str, new_content: str, reason: str) -> dict:
        r = self._client.put(f"/v1/memories/{memory_id}/correct", json={"new_content": new_content, "reason": reason})
        r.raise_for_status()
        return r.json()

    def purge(self, user_id: str, memory_id: str | None, topic: str | None, reason: str) -> dict:
        if topic:
            # Search then purge each match.  Collect partial results so a
            # mid-batch failure doesn't lose the count of already-purged items.
            hits = self.search(user_id, topic, top_k=50)
            ids = [h["memory_id"] for h in hits]
            purged = 0
            errors: list[str] = []
            for mid in ids:
                try:
                    r = self._client.delete(
                        f"/v1/memories/{mid}",
                        params={"reason": reason or f"topic purge: {topic}"},
                    )
                    r.raise_for_status()
                    purged += r.json().get("purged", 1)
                except Exception as e:
                    errors.append(f"{mid}: {e}")
            result: dict = {"purged": purged}
            if errors:
                result["errors"] = errors
            return result
        elif memory_id:
            r = self._client.delete(f"/v1/memories/{memory_id}", params={"reason": reason})
            r.raise_for_status()
            return r.json()
        return {"purged": 0}

    def profile(self, user_id: str) -> dict:
        r = self._client.get(f"/v1/profiles/{user_id}")
        r.raise_for_status()
        return r.json()

    def search(self, user_id: str, query: str, top_k: int) -> list[dict]:
        r = self._client.post("/v1/memories/search", json={"query": query, "top_k": top_k})
        r.raise_for_status()
        return r.json()

    def governance(self, user_id: str, force: bool = False) -> dict:
        r = self._client.post("/v1/memories/governance", json={"user_id": user_id, "force": force})
        r.raise_for_status()
        return r.json()

    def consolidate(self, user_id: str, force: bool = False) -> dict:
        r = self._client.post("/v1/memories/consolidate", json={"user_id": user_id, "force": force})
        r.raise_for_status()
        return r.json()

    def reflect(self, user_id: str, force: bool = False) -> dict:
        r = self._client.post("/v1/memories/reflect", json={"user_id": user_id, "force": force})
        r.raise_for_status()
        return r.json()

    def rebuild_index(self, table: str) -> str:
        r = self._client.post("/v1/memories/rebuild-index", json={"table": table})
        r.raise_for_status()
        return r.json().get("message", str(r.json()))

    def health_warnings(self, user_id: str) -> list[str]:
        return []  # Not available via HTTP yet

    def snapshot_create(self, user_id: str, name: str, description: str) -> dict:
        return {"error": "Not available via HTTP"}

    def snapshot_list(self, user_id: str) -> list[dict]:
        return []

    def snapshot_rollback(self, user_id: str, name: str) -> dict:
        return {"error": "Not available via HTTP"}

    def branch_create(self, user_id: str, name: str, from_snapshot: str | None, from_timestamp: str | None = None) -> dict:
        return {"error": "Not available via HTTP"}

    def branch_list(self, user_id: str) -> list[dict]:
        return []

    def branch_checkout(self, user_id: str, name: str) -> dict:
        return {"error": "Not available via HTTP"}

    def branch_delete(self, user_id: str, name: str) -> dict:
        return {"error": "Not available via HTTP"}

    def branch_merge(self, user_id: str, source: str, strategy: str) -> dict:
        return {"error": "Not available via HTTP"}


# ── MCP Server ────────────────────────────────────────────────────────

def create_server(backend: MemoryBackend, default_user: str = "default") -> FastMCP:
    """Create MCP server with memory tools."""

    server = FastMCP(
        "mo-memory",
        instructions=(
            "Persistent memory across conversations. "
            "\n\n"
            "MANDATORY RULES:\n"
            "1. ALWAYS call memory_retrieve with the user's first message BEFORE responding.\n"
            "   If the response includes ⚠️ Memory health warnings, inform the user and offer to help.\n"
            "2. AFTER each response, call memory_store for any new fact, preference, or decision.\n"
            "\n"
            "CRUD: memory_store, memory_retrieve, memory_correct, memory_purge, memory_profile, memory_search.\n"
            "memory_purge supports single ID or topic-based bulk delete (e.g. 'forget everything about X').\n"
            "MAINTENANCE (only when user asks): memory_governance, memory_consolidate, memory_reflect, memory_rebuild_index.\n"
            "\n"
            "memory_store types: semantic (default), profile, procedural, working, tool_result."
        ),
    )

    def _user(user_id: str | None) -> str:
        return user_id or default_user

    @server.tool()
    def memory_store(
        content: str,
        memory_type: str = "semantic",
        user_id: str | None = None,
        session_id: str | None = None,
    ) -> str:
        """Store a memory. Use for facts, preferences, decisions, or corrections the user shares.

        Args:
            content: The memory content to store.
            memory_type: One of: profile, semantic, procedural, working, tool_result. Default: semantic.
            user_id: User ID (optional, uses default if omitted).
            session_id: Session context (optional).
        """
        result = backend.store(_user(user_id), content, memory_type, session_id)
        return f"Stored memory {result['memory_id']}: {result['content']}"

    @server.tool()
    def memory_retrieve(
        query: str,
        top_k: int = 5,
        user_id: str | None = None,
        session_id: str | None = None,
    ) -> str:
        """Retrieve relevant memories for a query. Call this at conversation start or when context is needed.

        Args:
            query: What to search for in memories.
            top_k: Max number of memories to return (default 5).
            user_id: User ID (optional).
            session_id: Session context (optional). When set, prioritizes memories from this session.
                When None, searches across all sessions (include_cross_session=True).
                When set, the underlying retrieval strategy ranks session-scoped memories higher.
        """
        uid = _user(user_id)
        # Pass session_id to backend: if set, retrieval strategy will prioritize memories from this session.
        # If not set, retrieval searches across all sessions with cross-session inclusion enabled.
        results = backend.retrieve(uid, query, top_k, session_id=session_id)
        parts: list[str] = []
        if not results:
            parts.append("No relevant memories found.")
        else:
            lines = [f"- [{r.get('type', 'fact')}] {r['content']}" for r in results]
            parts.append(f"Found {len(results)} memories:\n" + "\n".join(lines))
        # Attach health warnings if any
        warnings = backend.health_warnings(uid)
        if warnings:
            parts.append("\n⚠️ Memory health:\n" + "\n".join(f"- {w}" for w in warnings))
        return "\n".join(parts)

    @server.tool()
    def memory_correct(
        memory_id: str,
        new_content: str,
        reason: str = "",
        user_id: str | None = None,
    ) -> str:
        """Correct an existing memory with updated information.

        Args:
            memory_id: ID of the memory to correct.
            new_content: The corrected content.
            reason: Why the correction is needed.
            user_id: User ID (optional).
        """
        result = backend.correct(_user(user_id), memory_id, new_content, reason)
        return f"Corrected → {result['memory_id']}: {result['content']}"

    @server.tool()
    def memory_purge(
        memory_id: str | None = None,
        topic: str | None = None,
        reason: str = "",
        user_id: str | None = None,
    ) -> str:
        """Delete memories. Use memory_id for a single memory, or topic to bulk-delete all memories matching a keyword.

        Args:
            memory_id: ID of a specific memory to delete.
            topic: Keyword/topic — finds and deletes all matching memories.
            reason: Why it should be deleted.
            user_id: User ID (optional).
        """
        if not memory_id and not topic:
            return "Provide either memory_id or topic."
        result = backend.purge(_user(user_id), memory_id, topic, reason)
        return f"Purged {result['purged']} memory(ies)."

    @server.tool()
    def memory_profile(
        user_id: str | None = None,
    ) -> str:
        """Get the user's memory-derived profile summary.

        Args:
            user_id: User ID (optional).
        """
        result = backend.profile(_user(user_id))
        profile = result.get("profile") or "No profile available yet."
        return f"Profile for {result['user_id']}:\n{profile}"

    @server.tool()
    def memory_search(
        query: str,
        top_k: int = 10,
        user_id: str | None = None,
    ) -> str:
        """Semantic search over all memories.

        Args:
            query: Search query.
            top_k: Max results (default 10).
            user_id: User ID (optional).
        """
        results = backend.search(_user(user_id), query, top_k)
        if not results:
            return "No memories found."
        lines = [f"- [{r.get('type', 'fact')}] ({r['memory_id']}) {r['content']}" for r in results]
        return f"Found {len(results)} memories:\n" + "\n".join(lines)

    @server.tool()
    def memory_governance(
        user_id: str | None = None,
        force: bool = False,
    ) -> str:
        """Run memory governance: quarantine low-confidence memories, clean stale data.

        Do NOT call proactively. Only call when user explicitly asks to
        "clean up memories", "run maintenance", or "check memory health".
        Has a 1-hour cooldown per user. Use force=True only if user insists.

        Args:
            user_id: User ID (optional).
            force: Skip cooldown (only when user explicitly requests).
        """
        result = backend.governance(_user(user_id), force=force)
        if result.get("skipped"):
            return f"Governance skipped (cooldown, {result['cooldown_remaining_s']}s remaining). Last result: {', '.join(f'{k}={v}' for k, v in result.items() if k not in ('skipped', 'cooldown_remaining_s', 'vector_index_health'))}"
        health = result.pop("vector_index_health", {})
        parts = [f"{k}={v}" for k, v in result.items()]
        msg = f"Governance done: {', '.join(parts)}"
        for table, h in health.items():
            if h.get("needs_rebuild") and not h.get("rebuilt"):
                msg += f"\n⚠️  {table}: IVF index needs rebuild (rows={h.get('total_rows')}, centroids={h['centroids']}, ratio={h.get('ratio')})"
            elif h.get("rebuilt"):
                msg += f"\n✅ {table}: IVF index rebuilt automatically"
            elif h.get("rebuild_error"):
                msg += f"\n❌ {table}: IVF rebuild failed: {h['rebuild_error']}"
        return msg

    @server.tool()
    def memory_consolidate(
        user_id: str | None = None,
        force: bool = False,
    ) -> str:
        """Run graph consolidation: detect contradicting memories, fix orphaned nodes, manage trust tiers.

        Do NOT call proactively. Only call when user explicitly asks to
        "check for conflicts", "consolidate memories", or "fix memory graph".
        Has a 30-minute cooldown per user. Use force=True only if user insists.

        Args:
            user_id: User ID (optional).
            force: Skip cooldown (only when user explicitly requests).
        """
        result = backend.consolidate(_user(user_id), force=force)
        if result.get("skipped"):
            return f"Consolidation skipped (cooldown, {result['cooldown_remaining_s']}s remaining)."
        parts = [f"{k}={v}" for k, v in result.items()]
        return f"Consolidation done: {', '.join(parts)}"

    @server.tool()
    def memory_reflect(
        user_id: str | None = None,
        force: bool = False,
    ) -> str:
        """Analyze memory clusters and synthesize high-level insights (scene nodes). Requires LLM.

        Do NOT call proactively. Only call when user explicitly asks to
        "reflect on memories", "find patterns", or "summarize what you know".
        Has a 2-hour cooldown per user. Use force=True only if user insists.
        This is the most expensive operation (LLM calls).

        Args:
            user_id: User ID (optional).
            force: Skip cooldown (only when user explicitly requests).
        """
        result = backend.reflect(_user(user_id), force=force)
        if result.get("skipped"):
            return f"Reflection skipped (cooldown, {result['cooldown_remaining_s']}s remaining)."
        if "error" in result:
            return f"Reflection failed: {result['error']}"
        return f"Reflection done: scenes_created={result['scenes_created']}, candidates_found={result['candidates_found']}"

    @server.tool()
    def memory_rebuild_index(
        table: str = "mem_memories",
        user_id: str | None = None,
    ) -> str:
        """Rebuild IVF vector index for a memory table with optimal centroid count.

        Only call when memory_governance reports 'needs_rebuild=True' for a table,
        or when user explicitly asks to rebuild the vector index.
        This operation is expensive (full table scan). Do NOT call proactively.

        Args:
            table: Table to rebuild. One of: 'mem_memories', 'memory_graph_nodes'.
            user_id: User ID (optional, unused but kept for consistency).
        """
        return backend.rebuild_index(table)

    # ── Snapshot tools ────────────────────────────────────────────────

    @server.tool()
    def memory_snapshot(
        name: str,
        description: str = "",
        user_id: str | None = None,
    ) -> str:
        """Create a named snapshot of current memory state.

        Args:
            name: Snapshot name (e.g. 'before_refactor').
            description: Optional description.
            user_id: User ID (optional).
        """
        result = backend.snapshot_create(_user(user_id), name, description)
        if "error" in result:
            return f"Error: {result['error']}"
        return f"Snapshot '{name}' created."

    @server.tool()
    def memory_snapshots(user_id: str | None = None) -> str:
        """List all memory snapshots.

        Args:
            user_id: User ID (optional).
        """
        snaps = backend.snapshot_list(_user(user_id))
        if not snaps:
            return "No snapshots found."
        lines = [f"  {s['name']} ({s['timestamp']})" for s in snaps[:20]]
        return f"Found {len(snaps)} snapshots:\n" + "\n".join(lines)

    @server.tool()
    def memory_rollback(
        name: str,
        user_id: str | None = None,
    ) -> str:
        """Restore memories to a previous snapshot. WARNING: changes after the snapshot will be lost.

        Args:
            name: Snapshot name to rollback to.
            user_id: User ID (optional).
        """
        result = backend.snapshot_rollback(_user(user_id), name)
        if "error" in result:
            return f"Error: {result['error']}"
        return f"Rolled back to snapshot '{name}'."

    # ── Branch tools ──────────────────────────────────────────────────

    @server.tool()
    def memory_branch(
        name: str,
        from_snapshot: str | None = None,
        from_timestamp: str | None = None,
        user_id: str | None = None,
    ) -> str:
        """Create a new memory branch for isolated experimentation.

        Args:
            name: Branch name (e.g. 'eval_postgres', 'experiment_a').
            from_snapshot: Branch from a named snapshot.
            from_timestamp: Branch from a point in time (e.g. '2026-03-09 12:00:00'). Must be within last 30 minutes.
            user_id: User ID (optional).

        If neither from_snapshot nor from_timestamp is given, branches from current state.
        """
        if from_snapshot and from_timestamp:
            return "Error: specify from_snapshot or from_timestamp, not both."
        result = backend.branch_create(_user(user_id), name, from_snapshot, from_timestamp)
        if "error" in result:
            return f"Error: {result['error']}"
        src = ""
        if from_snapshot:
            src = f" from snapshot '{from_snapshot}'"
        elif from_timestamp:
            src = f" from timestamp '{from_timestamp}'"
        return f"Branch '{name}' created{src}. Use memory_checkout to switch to it."

    @server.tool()
    def memory_branches(user_id: str | None = None) -> str:
        """List all memory branches.

        Args:
            user_id: User ID (optional).
        """
        branches = backend.branch_list(_user(user_id))
        if not branches:
            return "No branches."
        lines = [f"  {'* ' if b['active'] else '  '}{b['name']}" for b in branches]
        return "\n".join(lines)

    @server.tool()
    def memory_checkout(name: str, user_id: str | None = None) -> str:
        """Switch to a different memory branch.

        Args:
            name: Branch name to switch to (or 'main').
            user_id: User ID (optional).
        """
        result = backend.branch_checkout(_user(user_id), name)
        if "error" in result:
            return f"Error: {result['error']}"
        return f"Switched to branch '{name}'."

    @server.tool()
    def memory_branch_delete(name: str, user_id: str | None = None) -> str:
        """Delete a memory branch.

        Args:
            name: Branch name to delete.
            user_id: User ID (optional).
        """
        result = backend.branch_delete(_user(user_id), name)
        if "error" in result:
            return f"Error: {result['error']}"
        return f"Branch '{name}' deleted."

    @server.tool()
    def memory_merge(
        source: str,
        strategy: str = "append",
        user_id: str | None = None,
    ) -> str:
        """Merge a branch back into main.

        Args:
            source: Branch name to merge from.
            strategy: 'append' (skip duplicates) or 'replace' (overwrite duplicates).
            user_id: User ID (optional).
        """
        result = backend.branch_merge(_user(user_id), source, strategy)
        if "error" in result:
            return f"Error: {result['error']}"
        return f"Merged {result['merged']} memories from '{source}' (skipped {result['skipped']})."

    return server


# ── Entry point ───────────────────────────────────────────────────────

def main():
    import sys

    parser = argparse.ArgumentParser(description="mo-memory MCP server")
    parser.add_argument("--api-url", help="Memory service API URL (remote mode)")
    parser.add_argument("--db-url", help="Database URL for embedded mode (or set MO_MEMORY_DB_URL)")
    parser.add_argument("--token", help="Auth token for remote mode")
    parser.add_argument("--user", default="default", help="Default user ID")
    parser.add_argument("--transport", choices=["stdio", "sse"], default="stdio")
    args = parser.parse_args()

    # MCP stdio uses stdout for JSON-RPC — ALL logging MUST go to stderr.
    _stderr_handler = logging.StreamHandler(sys.stderr)
    _stderr_handler.setFormatter(logging.Formatter("%(name)s: %(message)s"))
    logging.root.handlers = [_stderr_handler]
    logging.root.setLevel(logging.WARNING)

    if args.api_url:
        backend: MemoryBackend = HTTPBackend(args.api_url, token=args.token)
    else:
        db_url = args.db_url or os.environ.get("MO_MEMORY_DB_URL")
        backend = EmbeddedBackend(db_url=db_url)

    server = create_server(backend, default_user=args.user)

    if args.transport == "sse":
        server.run(transport="sse")
    else:
        server.run(transport="stdio")


if __name__ == "__main__":
    main()
