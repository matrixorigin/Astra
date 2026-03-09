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
    def retrieve(self, user_id: str, query: str, top_k: int) -> list[dict]: ...
    def correct(self, user_id: str, memory_id: str, new_content: str, reason: str) -> dict: ...
    def purge(self, user_id: str, memory_id: str, reason: str) -> dict: ...
    def profile(self, user_id: str) -> dict: ...
    def search(self, user_id: str, query: str, top_k: int) -> list[dict]: ...
    def governance(self, user_id: str, force: bool = False) -> dict: ...
    def consolidate(self, user_id: str, force: bool = False) -> dict: ...
    def reflect(self, user_id: str, force: bool = False) -> dict: ...
    def rebuild_index(self, table: str) -> str: ...


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

    def retrieve(self, user_id: str, query: str, top_k: int) -> list[dict]:
        svc = self._create_service(self._db_factory, user_id=user_id)
        memories, _ = svc.retrieve(user_id, query, top_k=top_k)
        return [{"memory_id": m.memory_id, "content": m.content, "type": str(m.memory_type)} for m in memories]

    def correct(self, user_id: str, memory_id: str, new_content: str, reason: str) -> dict:
        editor = self._create_editor(self._db_factory, user_id=user_id, embed_client=self._embed_client)
        mem = editor.correct(user_id, memory_id, new_content, reason=reason)
        return {"memory_id": mem.memory_id, "content": mem.content}

    def purge(self, user_id: str, memory_id: str, reason: str) -> dict:
        editor = self._create_editor(self._db_factory, user_id=user_id, embed_client=self._embed_client)
        result = editor.purge(user_id, memory_ids=[memory_id], reason=reason)
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

    def retrieve(self, user_id: str, query: str, top_k: int) -> list[dict]:
        r = self._client.post("/v1/memories/retrieve", json={"query": query, "top_k": top_k})
        r.raise_for_status()
        return r.json()

    def correct(self, user_id: str, memory_id: str, new_content: str, reason: str) -> dict:
        r = self._client.put(f"/v1/memories/{memory_id}/correct", json={"new_content": new_content, "reason": reason})
        r.raise_for_status()
        return r.json()

    def purge(self, user_id: str, memory_id: str, reason: str) -> dict:
        r = self._client.delete(f"/v1/memories/{memory_id}", params={"reason": reason})
        r.raise_for_status()
        return r.json()

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


# ── MCP Server ────────────────────────────────────────────────────────

def create_server(backend: MemoryBackend, default_user: str = "default") -> FastMCP:
    """Create MCP server with memory tools."""

    server = FastMCP(
        "mo-memory",
        instructions=(
            "Memory service for AI coding tools. "
            "Use memory_store to save important facts, preferences, and decisions. "
            "Use memory_retrieve at the start of conversations to recall relevant context. "
            "Use memory_correct to fix outdated or wrong memories. "
            "Use memory_purge to delete sensitive or irrelevant memories."
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
    ) -> str:
        """Retrieve relevant memories for a query. Call this at conversation start or when context is needed.

        Args:
            query: What to search for in memories.
            top_k: Max number of memories to return (default 5).
            user_id: User ID (optional).
        """
        results = backend.retrieve(_user(user_id), query, top_k)
        if not results:
            return "No relevant memories found."
        lines = [f"- [{r.get('type', 'fact')}] {r['content']}" for r in results]
        return f"Found {len(results)} memories:\n" + "\n".join(lines)

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
        memory_id: str,
        reason: str = "",
        user_id: str | None = None,
    ) -> str:
        """Delete a memory permanently.

        Args:
            memory_id: ID of the memory to delete.
            reason: Why it should be deleted.
            user_id: User ID (optional).
        """
        result = backend.purge(_user(user_id), memory_id, reason)
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
