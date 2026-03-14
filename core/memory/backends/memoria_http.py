"""Memoria HTTP API client and adapter for CanonicalStorage interface."""

from __future__ import annotations

import logging
from datetime import datetime
from typing import Any, Optional

import httpx

from core.memory.types import Memory, MemoryType, TrustTier

logger = logging.getLogger(__name__)


class MemoriaHTTPClient:
    """HTTP client for Memoria REST API.

    Supports authentication via:
    - API Key (user-specific)
    - Master Key + X-Impersonate-User (admin mode)
    - No auth (development mode)
    """

    def __init__(
        self,
        base_url: str = "http://localhost:8000",
        api_key: Optional[str] = None,
        master_key: Optional[str] = None,
        timeout: float = 30.0,
    ):
        self.base_url = base_url.rstrip("/")
        self.api_key = api_key
        self.master_key = master_key
        self.timeout = timeout

        # Build headers
        headers = {"Content-Type": "application/json"}
        if api_key:
            headers["Authorization"] = f"Bearer {api_key}"
        elif master_key:
            headers["Authorization"] = f"Bearer {master_key}"

        self.client = httpx.Client(
            base_url=self.base_url,
            headers=headers,
            timeout=timeout,
        )

    def _get_headers(self, user_id: Optional[str] = None) -> dict[str, str]:
        """Get request headers with optional user impersonation."""
        headers = {"Content-Type": "application/json"}

        if self.api_key:
            headers["Authorization"] = f"Bearer {self.api_key}"
        elif self.master_key:
            headers["Authorization"] = f"Bearer {self.master_key}"
            if user_id:
                headers["X-Impersonate-User"] = user_id
        elif user_id:
            # Development mode: pass user_id directly
            headers["X-User-ID"] = user_id

        return headers

    # ── Memory CRUD ────────────────────────────────────────────────────

    def store(
        self,
        user_id: str,
        content: str,
        memory_type: str = "semantic",
        trust_tier: Optional[str] = None,
        session_id: Optional[str] = None,
        initial_confidence: float = 0.75,
        source: str = "api",
    ) -> dict[str, Any]:
        """Store a memory.

        POST /v1/memories
        """
        payload = {
            "content": content,
            "memory_type": memory_type,
            "trust_tier": trust_tier,
            "session_id": session_id,
            "initial_confidence": initial_confidence,
            "source": source,
        }

        resp = self.client.post(
            "/v1/memories",
            json={k: v for k, v in payload.items() if v is not None},
            headers=self._get_headers(user_id),
        )
        resp.raise_for_status()
        return resp.json()

    def batch_store(
        self,
        user_id: str,
        memories: list[dict[str, Any]],
    ) -> list[dict[str, Any]]:
        """Store multiple memories.

        POST /v1/memories/batch
        """
        resp = self.client.post(
            "/v1/memories/batch",
            json={"memories": memories},
            headers=self._get_headers(user_id),
        )
        resp.raise_for_status()
        return resp.json()

    def retrieve(
        self,
        user_id: str,
        query: str,
        top_k: int = 10,
        memory_types: Optional[list[str]] = None,
        session_id: Optional[str] = None,
        include_cross_session: bool = True,
    ) -> list[dict[str, Any]]:
        """Retrieve memories by query.

        POST /v1/memories/retrieve
        """
        payload = {
            "query": query,
            "top_k": top_k,
            "include_cross_session": include_cross_session,
        }
        if memory_types:
            payload["memory_types"] = memory_types
        if session_id:
            payload["session_id"] = session_id

        resp = self.client.post(
            "/v1/memories/retrieve",
            json=payload,
            headers=self._get_headers(user_id),
        )
        resp.raise_for_status()
        return resp.json()

    def search(
        self,
        user_id: str,
        query: str,
        top_k: int = 10,
    ) -> list[dict[str, Any]]:
        """Search memories (simpler retrieve).

        POST /v1/memories/search
        """
        resp = self.client.post(
            "/v1/memories/search",
            json={"query": query, "top_k": top_k},
            headers=self._get_headers(user_id),
        )
        resp.raise_for_status()
        return resp.json()

    def list_memories(
        self,
        user_id: str,
        memory_type: Optional[str] = None,
        limit: int = 100,
        cursor: Optional[str] = None,
    ) -> dict[str, Any]:
        """List memories with pagination.

        GET /v1/memories
        """
        params = {"limit": limit}
        if memory_type:
            params["memory_type"] = memory_type
        if cursor:
            params["cursor"] = cursor

        resp = self.client.get(
            "/v1/memories",
            params=params,
            headers=self._get_headers(user_id),
        )
        resp.raise_for_status()
        return resp.json()

    def correct(
        self,
        user_id: str,
        memory_id: str,
        new_content: str,
        reason: str = "",
    ) -> dict[str, Any]:
        """Correct a memory.

        PUT /v1/memories/{memory_id}/correct
        """
        resp = self.client.put(
            f"/v1/memories/{memory_id}/correct",
            json={"new_content": new_content, "reason": reason},
            headers=self._get_headers(user_id),
        )
        resp.raise_for_status()
        return resp.json()

    def correct_by_query(
        self,
        user_id: str,
        query: str,
        new_content: str,
        reason: str = "",
    ) -> dict[str, Any]:
        """Find and correct memory by query.

        POST /v1/memories/correct
        """
        resp = self.client.post(
            "/v1/memories/correct",
            json={"query": query, "new_content": new_content, "reason": reason},
            headers=self._get_headers(user_id),
        )
        resp.raise_for_status()
        return resp.json()

    def delete(
        self,
        user_id: str,
        memory_id: str,
        reason: str = "",
    ) -> dict[str, Any]:
        """Delete a memory.

        DELETE /v1/memories/{memory_id}
        """
        resp = self.client.delete(
            f"/v1/memories/{memory_id}",
            params={"reason": reason},
            headers=self._get_headers(user_id),
        )
        resp.raise_for_status()
        return resp.json()

    def purge(
        self,
        user_id: str,
        memory_ids: Optional[list[str]] = None,
        topic: Optional[str] = None,
        memory_types: Optional[list[str]] = None,
        before: Optional[datetime] = None,
        reason: str = "",
    ) -> dict[str, Any]:
        """Purge memories.

        POST /v1/memories/purge
        """
        payload = {"reason": reason}
        if memory_ids:
            payload["memory_ids"] = memory_ids
        if topic:
            payload["topic"] = topic
        if memory_types:
            payload["memory_types"] = memory_types
        if before:
            payload["before"] = before.isoformat()

        resp = self.client.post(
            "/v1/memories/purge",
            json=payload,
            headers=self._get_headers(user_id),
        )
        resp.raise_for_status()
        return resp.json()

    def observe_turn(
        self,
        user_id: str,
        messages: list[dict[str, Any]],
        source_event_ids: Optional[list[str]] = None,
    ) -> list[dict[str, Any]]:
        """Extract and store memories from conversation turn.

        POST /v1/memories/observe (if available) or emulate via retrieve+store
        """
        # Note: Memoria API doesn't have a direct observe endpoint
        # This would need to be implemented client-side or added to Memoria
        logger.warning("observe_turn not directly supported by Memoria API, using fallback")
        return []

    # ── Snapshots (Git-for-Data) ───────────────────────────────────────

    def create_snapshot(
        self,
        user_id: str,
        name: str,
        description: str = "",
    ) -> dict[str, Any]:
        """Create a snapshot.

        POST /v1/snapshots
        """
        resp = self.client.post(
            "/v1/snapshots",
            json={"name": name, "description": description},
            headers=self._get_headers(user_id),
        )
        resp.raise_for_status()
        return resp.json()

    def list_snapshots(
        self,
        user_id: str,
    ) -> list[dict[str, Any]]:
        """List snapshots.

        GET /v1/snapshots
        """
        resp = self.client.get(
            "/v1/snapshots",
            headers=self._get_headers(user_id),
        )
        resp.raise_for_status()
        return resp.json()

    def get_snapshot(
        self,
        user_id: str,
        name: str,
        limit: int = 50,
        offset: int = 0,
        detail: str = "brief",
    ) -> dict[str, Any]:
        """Get snapshot details.

        GET /v1/snapshots/{name}
        """
        resp = self.client.get(
            f"/v1/snapshots/{name}",
            params={"limit": limit, "offset": offset, "detail": detail},
            headers=self._get_headers(user_id),
        )
        resp.raise_for_status()
        return resp.json()

    def delete_snapshot(
        self,
        user_id: str,
        name: str,
    ) -> dict[str, Any]:
        """Delete a snapshot.

        DELETE /v1/snapshots/{name}
        """
        resp = self.client.delete(
            f"/v1/snapshots/{name}",
            headers=self._get_headers(user_id),
        )
        resp.raise_for_status()
        return resp.json()

    # ── Health ─────────────────────────────────────────────────────────

    def health_check(self) -> dict[str, Any]:
        """Check Memoria health.

        GET /health
        """
        resp = self.client.get("/health")
        resp.raise_for_status()
        return resp.json()

    def close(self) -> None:
        """Close HTTP client."""
        self.client.close()


class MemoriaStorage:
    """Adapter: Memoria HTTP API → CanonicalStorage-like interface.

    This allows mo-dev-agent to use Memoria as a drop-in replacement
    for the built-in CanonicalStorage.
    """

    def __init__(
        self,
        http_client: MemoriaHTTPClient,
        user_id: str,
    ):
        self.client = http_client
        self.user_id = user_id

    # ── Write path ─────────────────────────────────────────────────────

    def store(
        self,
        user_id: str,
        content: str,
        *,
        memory_type: MemoryType,
        source_event_ids: Optional[list[str]] = None,
        initial_confidence: float = 0.75,
        trust_tier: TrustTier = TrustTier.T3_INFERRED,
        session_id: Optional[str] = None,
    ) -> Memory:
        """Store a memory."""
        result = self.client.store(
            user_id=user_id,
            content=content,
            memory_type=memory_type.value,
            trust_tier=trust_tier.value,
            session_id=session_id,
            initial_confidence=initial_confidence,
        )
        return self._to_memory(result)

    def observe_turn(
        self,
        user_id: str,
        messages: list[dict[str, Any]],
        *,
        source_event_ids: Optional[list[str]] = None,
    ) -> list[Memory]:
        """Extract and store memories from conversation.

        Note: Memoria API doesn't have direct observe_turn, this is a placeholder.
        """
        # TODO: Implement client-side extraction or add to Memoria API
        logger.warning("observe_turn not implemented for Memoria backend")
        return []

    # ── Read path ──────────────────────────────────────────────────────

    def retrieve(
        self,
        user_id: str,
        query: str,
        *,
        query_embedding: Optional[list[float]] = None,
        top_k: int = 10,
        memory_types: Optional[list[MemoryType]] = None,
        session_id: str = "",
        include_cross_session: bool = True,
        **kwargs,
    ) -> tuple[list[Memory], Any]:
        """Retrieve memories."""
        type_names = [t.value for t in memory_types] if memory_types else None

        results = self.client.retrieve(
            user_id=user_id,
            query=query,
            top_k=top_k,
            memory_types=type_names,
            session_id=session_id or None,
            include_cross_session=include_cross_session,
        )

        memories = [self._to_memory(r) for r in results]
        meta = {"source": "memoria", "count": len(memories)}
        return memories, meta

    def get_profile(self, user_id: str) -> Optional[str]:
        """Get user profile."""
        # Try to retrieve profile-type memories
        results = self.client.retrieve(
            user_id=user_id,
            query="user profile preferences",
            top_k=1,
            memory_types=["profile"],
        )
        if results:
            return results[0].get("content")
        return None

    # ── Admin ──────────────────────────────────────────────────────────

    def correct(
        self,
        user_id: str,
        memory_id: str,
        new_content: str,
        *,
        reason: str = "",
    ) -> Memory:
        """Correct a memory."""
        result = self.client.correct(
            user_id=user_id,
            memory_id=memory_id,
            new_content=new_content,
            reason=reason,
        )
        return self._to_memory(result)

    def purge(
        self,
        user_id: str,
        memory_ids: Optional[list[str]] = None,
        **kwargs,
    ) -> Any:
        """Purge memories."""
        result = self.client.purge(
            user_id=user_id,
            memory_ids=memory_ids,
            reason=kwargs.get("reason", ""),
        )
        return type("PurgeResult", (), {"deactivated": result.get("purged", 0)})()

    # ── Helpers ────────────────────────────────────────────────────────

    def _to_memory(self, data: dict[str, Any]) -> Memory:
        """Convert API response to Memory object."""
        return Memory(
            memory_id=data.get("memory_id", ""),
            user_id=self.user_id,
            content=data.get("content", ""),
            memory_type=MemoryType(data.get("memory_type", "semantic")),
            confidence=data.get("confidence", 0.75),
            observed_at=data.get("observed_at"),
            retrieval_score=data.get("retrieval_score"),
        )
