"""Typed Observer — extracts typed memories with atomic contradiction detection.

Replaces the old Observer (which extracted untyped Observations).
Uses MemoryStore for persistence and contradiction resolution.

Contradiction detection uses DB-side L2_DISTANCE with IVF-flat index.
No in-memory fallback — at scale, loading all active memories into Python
is not viable, and silently falling back would mask DB errors.

Supports explain=True for EXPLAIN ANALYZE style execution stats.
"""

from __future__ import annotations

import json
import logging
import re
import time
import uuid
from datetime import datetime
from typing import Any, Optional

from sqlalchemy import text

from core.db_consumer import DbFactory
from core.memory.explain import ContradictionStats, ObserverStats
from core.memory.prompts import OBSERVER_EXTRACTION_PROMPT
from core.memory.store import MemoryStore
from core.memory.types import Memory, MemoryType

logger = logging.getLogger(__name__)

_VALID_TYPES = {t.value for t in MemoryType if t != MemoryType.WORKING}

# L2_DISTANCE threshold corresponding to ~0.85 cosine similarity for normalized vectors.
# For unit-norm vectors: L2² = 2(1 - cos_sim), so L2 = sqrt(2*(1-0.85)) ≈ 0.548.
_DEFAULT_L2_THRESHOLD = 0.55

# SQL: find the single closest active memory of the same type with different content.
# Uses DB-side L2_DISTANCE — accelerated by IVF-flat index on memories(embedding).
# MatrixOne syntax: ORDER BY L2_DISTANCE(...) ASC LIMIT N triggers index scan.
_CONTRADICTION_SQL = """\
SELECT m.memory_id, m.content, m.confidence,
    L2_DISTANCE(m.embedding, :query_vec) AS l2_dist
FROM memories m
WHERE m.user_id = :uid
    AND m.is_active = 1
    AND m.memory_type = :mtype
    AND m.embedding IS NOT NULL
    AND m.memory_id != :exclude_id
ORDER BY l2_dist ASC
LIMIT 1
"""


def _parse_json_array(text_str: str) -> list[dict[str, Any]]:
    """Robustly extract a JSON array from LLM output."""
    text_str = text_str.strip()
    try:
        result = json.loads(text_str)
        if isinstance(result, list):
            return result
    except json.JSONDecodeError:
        pass
    m = re.search(r"```(?:json)?\s*(\[.*?])\s*```", text_str, re.DOTALL)
    if m:
        try:
            return json.loads(m.group(1))
        except json.JSONDecodeError:
            pass
    m = re.search(r"\[.*]", text_str, re.DOTALL)
    if m:
        try:
            return json.loads(m.group(0))
        except json.JSONDecodeError:
            pass
    return []


class TypedObserver:
    """Extract typed memories from conversation turns.

    Flow: LLM extraction → embed → contradiction detection → store.

    Contradiction detection requires db_factory. Uses DB-side L2_DISTANCE
    with IVF-flat index (ORDER BY L2_DISTANCE ASC LIMIT 1). No in-memory
    fallback — at scale, loading all memories into Python is not viable.
    """

    def __init__(
        self,
        store: MemoryStore,
        llm_client: Any = None,
        embed_fn: Any = None,
        contradiction_threshold: float = 0.85,
        db_factory: Optional[DbFactory] = None,
    ):
        self.store = store
        self.llm = llm_client
        self.embed_fn = embed_fn
        self.contradiction_threshold = contradiction_threshold
        self._db_factory = db_factory
        # Convert cosine threshold to L2 threshold for DB queries.
        # For unit-norm vectors: L2 = sqrt(2 * (1 - cos_sim)).
        self._l2_threshold = (2.0 * (1.0 - contradiction_threshold)) ** 0.5

    def observe(
        self,
        user_id: str,
        messages: list[dict[str, Any]],
        source_event_ids: Optional[list[str]] = None,
        explain: bool = False,
    ) -> tuple[list[Memory], Optional[ObserverStats]]:
        """Extract and persist typed memories from a conversation turn.

        Returns:
            (memories, stats) — stats is None when explain=False.
        """
        start = time.time() if explain else 0
        stats = ObserverStats() if explain else None

        candidates = self.extract_candidates(user_id, messages, source_event_ids)
        if stats:
            stats.memories_extracted = len(candidates)

        results = []
        for m in candidates:
            mem, c_stats = self._store_with_contradiction_check(m, explain)
            results.append(mem)
            if stats and c_stats:
                if c_stats.found:
                    stats.memories_superseded += 1
                if stats.contradiction is None:
                    stats.contradiction = c_stats  # keep first

        if stats:
            stats.memories_stored = len(results)
            stats.total_ms = (time.time() - start) * 1000

        return results, stats

    def extract_candidates(
        self,
        user_id: str,
        messages: list[dict[str, Any]],
        source_event_ids: Optional[list[str]] = None,
    ) -> list[Memory]:
        """Extract candidate memories WITHOUT persisting. Returns in-memory objects.

        Used by the pipeline to separate extraction from storage, allowing
        sandbox validation between the two steps.
        """
        if not self.llm:
            return []

        raw = self._extract_via_llm(messages)
        if not raw:
            return []

        now = datetime.utcnow()
        results = []

        for item in raw:
            mem = self._parse_item(item, user_id, source_event_ids or [], now)
            if not mem:
                continue

            if self.embed_fn:
                try:
                    mem.embedding = self.embed_fn(mem.content)
                except Exception as e:
                    logger.warning("Embedding failed: %s", e)

            results.append(mem)

        return results

    def persist_with_contradiction_check(self, mem: Memory, explain: bool = False) -> tuple[Memory, Optional[ContradictionStats]]:
        """Persist a single memory with contradiction detection. Public API for pipeline."""
        return self._store_with_contradiction_check(mem, explain)

    def observe_explicit(
        self,
        user_id: str,
        content: str,
        memory_type: MemoryType,
        confidence: float = 0.9,
        source_event_ids: Optional[list[str]] = None,
        explain: bool = False,
    ) -> tuple[Memory, Optional[ContradictionStats]]:
        """Directly write a memory (from MemoryWriteTool), skipping LLM extraction."""
        mem = Memory(
            memory_id=uuid.uuid4().hex,
            user_id=user_id,
            memory_type=memory_type,
            content=content,
            confidence=confidence,
            source_event_ids=source_event_ids or [],
            observed_at=datetime.utcnow(),
        )
        if self.embed_fn:
            try:
                mem.embedding = self.embed_fn(content)
            except Exception as e:
                logger.warning("Embedding failed: %s", e)

        return self._store_with_contradiction_check(mem, explain)

    def _extract_via_llm(self, messages: list[dict[str, Any]]) -> list[dict[str, Any]]:
        conv_text = "\n".join(
            f"[{m.get('role', 'unknown')}]: {m.get('content', '')[:500]}"
            for m in messages if m.get("content")
        )
        try:
            result = self.llm.chat_with_tools(
                messages=[
                    {"role": "system", "content": OBSERVER_EXTRACTION_PROMPT},
                    {"role": "user", "content": conv_text},
                ],
                tools=[], tool_choice="none",
            )
            return _parse_json_array(result.get("content", ""))
        except Exception as e:
            logger.warning("Observer LLM extraction failed: %s", e)
            return []

    def _parse_item(
        self, item: dict, user_id: str, source_event_ids: list[str], now: datetime,
    ) -> Optional[Memory]:
        if not isinstance(item, dict) or not item.get("content"):
            return None
        mtype_str = item.get("type", "episodic")
        if mtype_str not in _VALID_TYPES:
            mtype_str = "episodic"
        confidence = item.get("confidence", 0.7)
        if not isinstance(confidence, (int, float)):
            confidence = 0.7
        confidence = max(0.0, min(1.0, float(confidence)))

        return Memory(
            memory_id=uuid.uuid4().hex,
            user_id=user_id,
            memory_type=MemoryType(mtype_str),
            content=item["content"],
            confidence=confidence,
            source_event_ids=source_event_ids,
            observed_at=now,
        )

    def _store_with_contradiction_check(self, mem: Memory, explain: bool = False) -> tuple[Memory, Optional[ContradictionStats]]:
        """Check for contradicting memory and supersede if found, else create."""
        stats = ContradictionStats() if explain else None
        
        if mem.embedding is not None:
            contradiction, c_stats = self._find_contradiction(mem, explain)
            if stats and c_stats:
                stats.checked = c_stats.checked
                stats.query_ms = c_stats.query_ms
                stats.error = c_stats.error
            if contradiction:
                logger.info(
                    "Contradiction detected: '%s' supersedes '%s'",
                    mem.content[:60], contradiction.content[:60],
                )
                if stats:
                    stats.found = True
                    stats.superseded_id = contradiction.memory_id
                return self.store.supersede(contradiction.memory_id, mem), stats

        return self.store.create(mem), stats

    def _find_contradiction(self, new: Memory, explain: bool = False) -> tuple[Optional[Memory], Optional[ContradictionStats]]:
        """Find an existing memory that contradicts the new one.

        Uses DB-side L2_DISTANCE with IVF-flat index. Requires db_factory.
        Skips contradiction detection when no embedding or no db_factory.
        DB errors propagate — no silent fallback.
        """
        stats = ContradictionStats(checked=True) if explain else None
        
        if new.embedding is None or self._db_factory is None:
            if stats:
                stats.checked = False
            return None, stats

        vec_str = "[" + ",".join(str(v) for v in new.embedding) + "]"
        db = self._db_factory()
        start = time.time() if explain else 0
        try:
            row = db.execute(
                text(_CONTRADICTION_SQL),
                {
                    "query_vec": vec_str,
                    "uid": new.user_id,
                    "mtype": new.memory_type.value,
                    "exclude_id": new.memory_id,
                },
            ).fetchone()
        except Exception as e:
            if stats:
                stats.error = str(e)
                stats.query_ms = (time.time() - start) * 1000
            raise
        finally:
            db.close()

        if stats:
            stats.query_ms = (time.time() - start) * 1000

        if row is None:
            return None, stats

        if float(row.l2_dist) <= self._l2_threshold and row.content.strip() != new.content.strip():
            return Memory(
                memory_id=row.memory_id,
                user_id=new.user_id,
                memory_type=new.memory_type,
                content=row.content,
                confidence=row.confidence,
            ), stats
        return None, stats
