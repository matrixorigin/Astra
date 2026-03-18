"""MCP-style memory tools for edge runtime."""

from __future__ import annotations

import asyncio
import json
from typing import Any

from cli.tools.base import EdgeTool, SideEffect
from core.memory.types import Memory, MemoryType


def _memory_to_dict(memory: Memory) -> dict[str, Any]:
    return {
        "memory_id": memory.memory_id,
        "content": memory.content,
        "memory_type": memory.memory_type.value,
        "trust_tier": memory.trust_tier.value,
        "session_id": memory.session_id,
        "created_at": memory.created_at.isoformat() if memory.created_at else None,
        "observed_at": memory.observed_at.isoformat() if memory.observed_at else None,
        "retrieval_score": memory.retrieval_score,
    }


class MemoryRetrieveTool(EdgeTool):
    name = "memory_retrieve"
    description = (
        "Retrieve relevant memories for the current task or user question. "
        "Use when you need to recall prior facts, decisions, preferences, or prior work."
    )
    parameters = {
        "type": "object",
        "properties": {
            "query": {"type": "string", "description": "Semantic query for recall"},
            "top_k": {"type": "integer", "default": 5, "description": "Max results"},
            "session_id": {
                "type": "string",
                "description": "Optional current session to bias retrieval",
            },
            "explain": {
                "description": "Optional retrieval debug mode",
                "oneOf": [{"type": "boolean"}, {"type": "string"}],
            },
        },
        "required": ["query"],
    }
    side_effect = SideEffect.READ

    async def execute(
        self,
        query: str,
        top_k: int = 5,
        session_id: str | None = None,
        explain: bool | str = False,
        **kwargs: Any,
    ) -> str:
        from core.memory.backends import get_memoria_storage

        user_id = kwargs.get("user_id", "")

        def _run_sync() -> str:
            svc = get_memoria_storage(user_id)
            memories, stats = svc.retrieve(
                user_id=user_id,
                query=query,
                top_k=top_k,
                session_id=session_id or "",
                explain=explain,
            )
            return json.dumps(
                {
                    "results": [_memory_to_dict(m) for m in memories],
                    "explain": stats,
                },
                ensure_ascii=False,
            )

        return await asyncio.to_thread(_run_sync)


class MemorySearchTool(EdgeTool):
    name = "memory_search"
    description = (
        "Browse memory by semantic search. Use when exploring what is known about a topic, "
        "instead of asking for the single most relevant recall set."
    )
    parameters = {
        "type": "object",
        "properties": {
            "query": {"type": "string", "description": "Search query"},
            "top_k": {"type": "integer", "default": 10, "description": "Max results"},
        },
        "required": ["query"],
    }
    side_effect = SideEffect.READ

    async def execute(self, query: str, top_k: int = 10, **kwargs: Any) -> str:
        from core.memory.backends import get_memoria_storage

        user_id = kwargs.get("user_id", "")

        def _run_sync() -> str:
            svc = get_memoria_storage(user_id)
            results = svc.client.search(user_id=user_id, query=query, top_k=top_k)
            return json.dumps({"results": results}, ensure_ascii=False)

        return await asyncio.to_thread(_run_sync)


class MemoryProfileTool(EdgeTool):
    name = "memory_profile"
    description = (
        "Get the synthesized memory profile for the current user. "
        "Use when the user asks what you know about them or their standing preferences."
    )
    parameters = {
        "type": "object",
        "properties": {},
    }
    side_effect = SideEffect.READ

    async def execute(self, **kwargs: Any) -> str:
        from core.memory.backends import get_memoria_storage

        user_id = kwargs.get("user_id", "")

        def _run_sync() -> str:
            svc = get_memoria_storage(user_id)
            data = svc.client.get_profile(user_id)
            return json.dumps(data, ensure_ascii=False)

        return await asyncio.to_thread(_run_sync)


class MemoryStoreTool(EdgeTool):
    name = "memory_store"
    description = (
        "Store a durable memory. Use for user preferences, facts, decisions, workflows, "
        "or task context that should survive beyond the current turn."
    )
    parameters = {
        "type": "object",
        "properties": {
            "content": {"type": "string", "description": "Memory content"},
            "memory_type": {
                "type": "string",
                "enum": ["semantic", "profile", "procedural", "working", "tool_result"],
                "default": "semantic",
                "description": "Memory type",
            },
            "session_id": {
                "type": "string",
                "description": "Optional session that produced this memory",
            },
        },
        "required": ["content"],
    }
    side_effect = SideEffect.WRITE

    async def execute(
        self,
        content: str,
        memory_type: str = "semantic",
        session_id: str | None = None,
        **kwargs: Any,
    ) -> str:
        from core.memory.backends import get_memoria_storage

        user_id = kwargs.get("user_id", "")

        def _run_sync() -> str:
            svc = get_memoria_storage(user_id)
            memory = svc.store(
                user_id=user_id,
                content=content,
                memory_type=MemoryType(memory_type),
                session_id=session_id,
            )
            return json.dumps(_memory_to_dict(memory), ensure_ascii=False)

        return await asyncio.to_thread(_run_sync)


class MemoryCorrectTool(EdgeTool):
    name = "memory_correct"
    description = (
        "Correct an existing memory. Use when a stored fact or preference is wrong or needs updating. "
        "Prefer query-based correction when you don't know the memory_id."
    )
    parameters = {
        "type": "object",
        "properties": {
            "memory_id": {"type": "string", "description": "Memory ID to correct"},
            "query": {"type": "string", "description": "Semantic query to locate the memory"},
            "new_content": {"type": "string", "description": "Corrected content"},
            "reason": {"type": "string", "description": "Why the memory is being corrected"},
        },
        "required": ["new_content"],
    }
    side_effect = SideEffect.WRITE

    async def execute(
        self,
        new_content: str,
        memory_id: str | None = None,
        query: str | None = None,
        reason: str = "",
        **kwargs: Any,
    ) -> str:
        from core.memory.backends import get_memoria_storage

        user_id = kwargs.get("user_id", "")

        def _run_sync() -> str:
            if not memory_id and not query:
                return json.dumps(
                    {"status": "error", "error": "memory_id or query required"},
                    ensure_ascii=False,
                )
            svc = get_memoria_storage(user_id)
            if query:
                result = svc.client.correct_by_query(
                    user_id=user_id,
                    query=query,
                    new_content=new_content,
                    reason=reason,
                )
                return json.dumps(result, ensure_ascii=False)
            memory = svc.correct(
                user_id=user_id,
                memory_id=memory_id or "",
                new_content=new_content,
                reason=reason,
            )
            return json.dumps(_memory_to_dict(memory), ensure_ascii=False)

        return await asyncio.to_thread(_run_sync)


class MemoryPurgeTool(EdgeTool):
    name = "memory_purge"
    description = (
        "Delete memories by explicit ID or by topic keyword. "
        "Use only when the user explicitly asks to forget something or reset stale working memory."
    )
    parameters = {
        "type": "object",
        "properties": {
            "memory_id": {
                "type": "string",
                "description": "Single ID or comma-separated list of IDs",
            },
            "topic": {"type": "string", "description": "Topic keyword for bulk purge"},
            "reason": {"type": "string", "description": "Why the memory is being purged"},
        },
    }
    side_effect = SideEffect.WRITE

    async def execute(
        self,
        memory_id: str | None = None,
        topic: str | None = None,
        reason: str = "",
        **kwargs: Any,
    ) -> str:
        from core.memory.backends import get_memoria_storage

        user_id = kwargs.get("user_id", "")

        def _run_sync() -> str:
            if not memory_id and not topic:
                return json.dumps(
                    {"status": "error", "error": "memory_id or topic required"},
                    ensure_ascii=False,
                )
            svc = get_memoria_storage(user_id)
            memory_ids = None
            if memory_id:
                memory_ids = [mid.strip() for mid in memory_id.split(",") if mid.strip()]
            result = svc.purge(
                user_id=user_id,
                memory_ids=memory_ids,
                topic=topic,
                reason=reason,
            )
            return json.dumps({"purged": getattr(result, "deactivated", 0)}, ensure_ascii=False)

        return await asyncio.to_thread(_run_sync)


def register_memory_mcp_tools(router: Any) -> None:
    from core.memory.backends import get_memory_backend_capabilities

    capabilities = get_memory_backend_capabilities()
    for tool in (
        MemoryRetrieveTool(),
        MemorySearchTool(),
        MemoryProfileTool(),
        MemoryStoreTool(),
        MemoryCorrectTool(),
        MemoryPurgeTool(),
    ):
        if capabilities.supports_tool(tool.name):
            router.register(tool)
