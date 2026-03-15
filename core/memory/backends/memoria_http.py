"""Memoria HTTP API client and adapter for CanonicalStorage interface."""

from __future__ import annotations

import logging
from datetime import datetime
from typing import Any, Optional

import httpx

from core.memory.interfaces import GovernanceReport, HealthReport
from core.memory.types import Memory, MemoryType, TrustTier

logger = logging.getLogger(__name__)


class MemoriaHTTPClient:
    """HTTP client for Memoria REST API.

    Uses master key + X-Impersonate-User for all requests.
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
        self.client = httpx.Client(
            base_url=self.base_url,
            timeout=timeout,
            trust_env=False,  # ignore http_proxy env vars for service-to-service calls
        )

    def _headers(self, user_id: Optional[str] = None) -> dict[str, str]:
        headers: dict[str, str] = {}
        if self.api_key:
            headers["Authorization"] = f"Bearer {self.api_key}"
        elif self.master_key:
            headers["Authorization"] = f"Bearer {self.master_key}"
            if user_id:
                headers["X-Impersonate-User"] = user_id
        return headers

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
        payload: dict[str, Any] = {
            "content": content,
            "memory_type": memory_type,
            "initial_confidence": initial_confidence,
            "source": source,
        }
        if trust_tier:
            payload["trust_tier"] = trust_tier
        if session_id:
            payload["session_id"] = session_id
        resp = self.client.post("/v1/memories", json=payload, headers=self._headers(user_id))
        resp.raise_for_status()
        return resp.json()

    def batch_store(self, user_id: str, memories: list[dict[str, Any]]) -> list[dict[str, Any]]:
        resp = self.client.post(
            "/v1/memories/batch", json={"memories": memories}, headers=self._headers(user_id)
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
        explain: bool | str = False,
    ) -> dict[str, Any]:
        payload: dict[str, Any] = {
            "query": query,
            "top_k": top_k,
            "include_cross_session": include_cross_session,
        }
        if explain:
            payload["explain"] = explain if isinstance(explain, str) else "basic"
        if memory_types:
            payload["memory_types"] = memory_types
        if session_id:
            payload["session_id"] = session_id
        resp = self.client.post(
            "/v1/memories/retrieve", json=payload, headers=self._headers(user_id)
        )
        resp.raise_for_status()
        return resp.json()

    def search(self, user_id: str, query: str, top_k: int = 10) -> list[dict[str, Any]]:
        resp = self.client.post(
            "/v1/memories/search",
            json={"query": query, "top_k": top_k},
            headers=self._headers(user_id),
        )
        resp.raise_for_status()
        data = resp.json()
        return data.get("results", []) if isinstance(data, dict) else data

    def list_memories(
        self,
        user_id: str,
        memory_type: Optional[str] = None,
        limit: int = 100,
        cursor: Optional[str] = None,
    ) -> dict[str, Any]:
        params: dict[str, Any] = {"limit": limit}
        if memory_type:
            params["memory_type"] = memory_type
        if cursor:
            params["cursor"] = cursor
        resp = self.client.get("/v1/memories", params=params, headers=self._headers(user_id))
        resp.raise_for_status()
        return resp.json()

    def get_memory(self, user_id: str, memory_id: str) -> Optional[dict[str, Any]]:
        """Get a single memory by ID via GET /memories/{id}. Returns None if not found."""
        resp = self.client.get(f"/v1/memories/{memory_id}", headers=self._headers(user_id))
        if resp.status_code == 404:
            return None
        resp.raise_for_status()
        return resp.json()

    def correct(
        self, user_id: str, memory_id: str, new_content: str, reason: str = ""
    ) -> dict[str, Any]:
        resp = self.client.put(
            f"/v1/memories/{memory_id}/correct",
            json={"new_content": new_content, "reason": reason},
            headers=self._headers(user_id),
        )
        resp.raise_for_status()
        return resp.json()

    def correct_by_query(
        self, user_id: str, query: str, new_content: str, reason: str = ""
    ) -> dict[str, Any]:
        resp = self.client.post(
            "/v1/memories/correct",
            json={"query": query, "new_content": new_content, "reason": reason},
            headers=self._headers(user_id),
        )
        resp.raise_for_status()
        return resp.json()

    def delete(self, user_id: str, memory_id: str, reason: str = "") -> dict[str, Any]:
        resp = self.client.delete(
            f"/v1/memories/{memory_id}", params={"reason": reason}, headers=self._headers(user_id)
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
        payload: dict[str, Any] = {"reason": reason}
        if memory_ids:
            payload["memory_ids"] = memory_ids
        if topic:
            payload["topic"] = topic
        if memory_types:
            payload["memory_types"] = memory_types
        if before:
            payload["before"] = before.isoformat()
        resp = self.client.post("/v1/memories/purge", json=payload, headers=self._headers(user_id))
        resp.raise_for_status()
        return resp.json()

    def observe_turn(
        self,
        user_id: str,
        messages: list[dict[str, Any]],
        source_event_ids: Optional[list[str]] = None,
        session_id: Optional[str] = None,
    ) -> list[dict[str, Any]]:
        """Extract and store memories from conversation turn. POST /v1/observe"""
        payload: dict[str, Any] = {"messages": messages}
        if source_event_ids:
            payload["source_event_ids"] = source_event_ids
        if session_id:
            payload["session_id"] = session_id
        resp = self.client.post("/v1/observe", json=payload, headers=self._headers(user_id))
        resp.raise_for_status()
        data = resp.json()
        # API returns {"memories": [...], "warning": "..."}
        return data.get("memories", []) if isinstance(data, dict) else data

    def create_session_summary(
        self,
        user_id: str,
        session_id: str,
        *,
        messages: Optional[list[dict[str, Any]]] = None,
        mode: str = "full",
        sync: bool = False,
        focus_topics: Optional[list[str]] = None,
        max_items: int = 5,
        generate_embedding: bool = True,
    ) -> dict[str, Any]:
        payload: dict[str, Any] = {
            "mode": mode,
            "sync": sync,
            "max_items": max_items,
            "generate_embedding": generate_embedding,
        }
        if focus_topics:
            payload["focus_topics"] = focus_topics
        if messages:
            payload["messages"] = messages
        resp = self.client.post(
            f"/v1/sessions/{session_id}/summary", json=payload, headers=self._headers(user_id)
        )
        resp.raise_for_status()
        content = resp.content.strip()
        if not content:
            return {"session_id": session_id}
        return resp.json()

    def consolidate(self, user_id: str, force: bool = False) -> dict[str, Any]:
        resp = self.client.post(
            "/v1/consolidate", params={"force": force}, headers=self._headers(user_id)
        )
        resp.raise_for_status()
        return resp.json()

    def reflect(self, user_id: str, force: bool = False) -> dict[str, Any]:
        resp = self.client.post(
            "/v1/reflect", params={"force": force}, headers=self._headers(user_id)
        )
        resp.raise_for_status()
        return resp.json()

    def get_profile(self, user_id: str) -> dict[str, Any]:
        resp = self.client.get("/v1/profiles/me", headers=self._headers(user_id))
        resp.raise_for_status()
        return resp.json()

    def create_snapshot(self, user_id: str, name: str, description: str = "") -> dict[str, Any]:
        resp = self.client.post(
            "/v1/snapshots",
            json={"name": name, "description": description},
            headers=self._headers(user_id),
        )
        resp.raise_for_status()
        return resp.json()

    def list_snapshots(self, user_id: str) -> list[dict[str, Any]]:
        resp = self.client.get("/v1/snapshots", headers=self._headers(user_id))
        resp.raise_for_status()
        # Handle empty response body (some servers return 200 with empty body)
        content = resp.content.strip()
        if not content:
            return []
        return resp.json()

    def get_snapshot(
        self, user_id: str, name: str, limit: int = 50, offset: int = 0, detail: str = "brief"
    ) -> dict[str, Any]:
        resp = self.client.get(
            f"/v1/snapshots/{name}",
            params={"limit": limit, "offset": offset, "detail": detail},
            headers=self._headers(user_id),
        )
        resp.raise_for_status()
        return resp.json()

    def delete_snapshot(self, user_id: str, name: str) -> dict[str, Any]:
        resp = self.client.delete(f"/v1/snapshots/{name}", headers=self._headers(user_id))
        resp.raise_for_status()
        # Handle empty response body (204 No Content)
        content = resp.content.strip()
        if not content:
            return {"name": name, "deleted": True}
        return resp.json()

    def health_check(self) -> dict[str, Any]:
        resp = self.client.get("/health")
        resp.raise_for_status()
        return resp.json()

    def close(self) -> None:
        self.client.close()


class MemoriaStorage:
    """Adapter: Memoria HTTP API → CanonicalStorage-like interface."""

    def __init__(self, http_client: MemoriaHTTPClient, user_id: str):
        self.client = http_client
        self.user_id = user_id

    # ── Write ─────────────────────────────────────────────────────────

    def store(
        self,
        user_id: str,
        content: str,
        *,
        memory_type: MemoryType,
        source_event_ids: Optional[list[str]] = None,
        initial_confidence: float = 0.75,
        trust_tier: TrustTier = TrustTier.T3,
        session_id: Optional[str] = None,
        **kwargs  # Accept additional arguments
    ) -> Memory:
        result = self.client.store(
            user_id=user_id,
            content=content,
            memory_type=memory_type.value,
            trust_tier=trust_tier.value,
            session_id=session_id,
            initial_confidence=initial_confidence,
        )
        return self._to_memory(result, user_id)

    def batch_store(
        self,
        memories: list[Memory],
    ) -> list[Memory]:
        """Batch store memories via Memoria HTTP API."""
        if not memories:
            return []

        # Convert Memory objects to dicts for API
        memory_dicts = []
        for mem in memories:
            mem_dict = {
                "content": mem.content,
                "memory_type": mem.memory_type.value
                if hasattr(mem.memory_type, "value")
                else str(mem.memory_type),
                "trust_tier": mem.trust_tier.value
                if hasattr(mem.trust_tier, "value")
                else str(mem.trust_tier),
                "initial_confidence": mem.initial_confidence,
                "source": "batch_inject",
            }
            if mem.session_id:
                mem_dict["session_id"] = mem.session_id
            memory_dicts.append(mem_dict)

        # Get user_id from first memory
        user_id = memories[0].user_id
        results = self.client.batch_store(user_id, memory_dicts)

        # Convert results back to Memory objects
        return [self._to_memory(r, user_id) for r in results]

    def observe_turn(
        self,
        user_id: str,
        messages: list[dict[str, Any]],
        *,
        source_event_ids: Optional[list[str]] = None,
        session_id: Optional[str] = None,
    ) -> list[Memory]:
        results = self.client.observe_turn(
            user_id, messages,
            source_event_ids=source_event_ids,
            session_id=session_id,
        )
        return [self._to_memory(r, user_id) for r in results]

    def request_session_summary(
        self,
        user_id: str,
        session_id: str,
        messages: list[dict[str, Any]],
        *,
        mode: str = "full",
        sync: bool = False,
        focus_topics: Optional[list[str]] = None,
        max_items: int = 5,
        generate_embedding: bool = True,
    ) -> dict[str, Any]:
        return self.client.create_session_summary(
            user_id=user_id,
            session_id=session_id,
            messages=messages,
            mode=mode,
            sync=sync,
            focus_topics=focus_topics,
            max_items=max_items,
            generate_embedding=generate_embedding,
        )

    def run_pipeline(
        self,
        user_id: str,
        messages: list[dict[str, Any]],
        *,
        source_event_ids: Optional[list[str]] = None,
        session_id: Optional[str] = None,
        **kwargs: Any,
    ) -> Any:
        memories = self.observe_turn(
            user_id, messages,
            source_event_ids=source_event_ids,
            session_id=session_id,
        )
        return type(
            "PipelineResult",
            (),
            {
                "memories_extracted": len(memories),
                "memories_stored": len(memories),
                "errors": [],
            },
        )()

    def create_memory(self, memory: Memory) -> Memory:
        return self.store(
            memory.user_id,
            memory.content,
            memory_type=memory.memory_type,
            initial_confidence=memory.initial_confidence,
            trust_tier=memory.trust_tier,
            session_id=memory.session_id,
        )

    def update_memory_content(self, memory_id: str, content: str) -> None:
        self.client.correct(self.user_id, memory_id, content, reason="content update")

    def update_memory_embedding(self, memory_id: str) -> None:
        # Memoria handles embeddings server-side
        pass

    def invalidate_profile(self, user_id: str) -> None:
        # Memoria handles profile caching server-side
        pass

    def generate_session_summary(
        self, user_id: str, session_id: str, messages: list[dict[str, Any]]
    ) -> Optional[Memory]:
        # Memoria handles session summarization server-side
        return None

    def check_and_summarize(
        self,
        user_id: str,
        session_id: str,
        messages: list[dict[str, Any]],
        turn_count: int,
        session_start: Any,
    ) -> Optional[Memory]:
        return None

    # ── Read ──────────────────────────────────────────────────────────

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
        explain: bool | str = False,
        **kwargs: Any,
    ) -> tuple[list[Memory], Any]:
        type_names = [t.value for t in memory_types] if memory_types else None
        result = self.client.retrieve(
            user_id=user_id,
            query=query,
            top_k=top_k,
            memory_types=type_names,
            session_id=session_id or None,
            include_cross_session=include_cross_session,
            explain=explain,
        )
        memories_data = result.get("results", []) if isinstance(result, dict) else []
        explain_info = result.get("explain", {"path": "unknown", "count": len(memories_data)})
        explain_info["source"] = "memoria"
        memories = [self._to_memory(r, user_id) for r in memories_data]
        return memories, explain_info

    def get_profile(self, user_id: str) -> Optional[str]:
        try:
            data = self.client.get_profile(user_id)
            return data.get("profile")
        except Exception:
            return None

    def get_memory(self, memory_id: str) -> Optional[Memory]:
        try:
            data = self.client.get_memory(self.user_id, memory_id)
        except Exception as e:
            logger.warning("get_memory %s failed: %s", memory_id, e)
            return None
        return self._to_memory(data, self.user_id) if data else None

    def list_active(
        self,
        user_id: str,
        memory_type: Optional[MemoryType] = None,
        limit: Optional[int] = None,
        load_embedding: bool = True,
    ) -> list[Memory]:
        result = self.client.list_memories(
            user_id,
            memory_type=memory_type.value if memory_type else None,
            limit=limit or 100,
        )
        return [self._to_memory(r, user_id) for r in result.get("items", [])]

    # ── Admin / Governance ────────────────────────────────────────────

    def correct(
        self, user_id: str, memory_id: str, new_content: str, *, reason: str = ""
    ) -> Memory:
        result = self.client.correct(
            user_id=user_id, memory_id=memory_id, new_content=new_content, reason=reason
        )
        return self._to_memory(result, user_id)

    def purge(self, user_id: str, memory_ids: Optional[list[str]] = None, **kwargs: Any) -> Any:
        # Extract memory_types from kwargs if provided
        memory_types = kwargs.get("memory_types")
        if memory_types:
            memory_types = [mt.value if hasattr(mt, "value") else str(mt) for mt in memory_types]

        result = self.client.purge(
            user_id=user_id,
            memory_ids=memory_ids,
            topic=kwargs.get("topic"),
            memory_types=memory_types,
            reason=kwargs.get("reason", ""),
        )
        # Handle different response formats from Memoria API
        deactivated = result.get("purged") or result.get("deactivated") or result.get("count") or 0
        return type("PurgeResult", (), {"deactivated": deactivated})()

    def run_governance(self, user_id: str) -> GovernanceReport:
        try:
            self.client.consolidate(user_id)
        except Exception as e:
            logger.warning("Governance consolidate failed: %s", e)
        return GovernanceReport()

    def health_check(self, user_id: str) -> HealthReport:
        try:
            data = self.client.health_check()
            ok = data.get("status") == "ok"
        except Exception:
            ok = False
        return HealthReport(total=0, active=0, inactive=0)

    def run_hourly(self) -> GovernanceReport:
        return GovernanceReport()

    def run_daily_all(self) -> GovernanceReport:
        return GovernanceReport()

    def run_weekly(self) -> GovernanceReport:
        return GovernanceReport()

    def get_reflection_candidates(self, user_id: str, *, since_hours: int = 24) -> list[Any]:
        return []

    def consolidate(self, user_id: str) -> Any:
        try:
            return self.client.consolidate(user_id)
        except Exception as e:
            logger.warning("Consolidate failed: %s", e)
            return {}

    # ── Helpers ───────────────────────────────────────────────────────

    def _to_memory(self, data: dict[str, Any], user_id: str) -> Memory:
        observed_at = data.get("observed_at")
        if isinstance(observed_at, str):
            try:
                from datetime import timezone

                observed_at = datetime.fromisoformat(observed_at)
                if observed_at.tzinfo is None:
                    observed_at = observed_at.replace(tzinfo=timezone.utc)
            except ValueError:
                observed_at = None

        trust_tier_raw = data.get("trust_tier")
        try:
            trust_tier = TrustTier(trust_tier_raw) if trust_tier_raw else TrustTier.T3
        except ValueError:
            trust_tier = TrustTier.T3

        memory_type_raw = data.get("memory_type", "semantic")
        try:
            memory_type = MemoryType(memory_type_raw)
        except ValueError:
            memory_type = MemoryType.SEMANTIC

        return Memory(
            memory_id=data.get("memory_id", ""),
            user_id=user_id,
            content=data.get("content", ""),
            memory_type=memory_type,
            initial_confidence=data.get("initial_confidence") or data.get("confidence") or 0.75,
            observed_at=observed_at,
            trust_tier=trust_tier,
            session_id=data.get("session_id"),
            retrieval_score=data.get("retrieval_score"),
        )

    def purge_all(self, user_id: str) -> dict:
        """Purge all memories for a user via Memoria API."""
        try:
            result = self.purge(
                user_id=user_id,
                reason="purge_all",
            )
            return {"status": "success", "purged": getattr(result, "deactivated", 0)}
        except Exception as e:
            return {"status": "error", "message": str(e)}
