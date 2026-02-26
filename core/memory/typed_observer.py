"""Typed Observer — extracts typed memories with atomic contradiction detection.

Replaces the old Observer (which extracted untyped Observations).
Uses MemoryStore for persistence and contradiction resolution.
"""

from __future__ import annotations

import json
import logging
import re
import uuid
from datetime import datetime
from typing import Any, Optional

from core.db_consumer import DbConsumer, DbFactory
from core.memory.prompts import OBSERVER_EXTRACTION_PROMPT
from core.memory.store import MemoryStore
from core.memory.types import Memory, MemoryType

logger = logging.getLogger(__name__)

_VALID_TYPES = {t.value for t in MemoryType if t != MemoryType.WORKING}


def _parse_json_array(text: str) -> list[dict[str, Any]]:
    """Robustly extract a JSON array from LLM output."""
    text = text.strip()
    try:
        result = json.loads(text)
        if isinstance(result, list):
            return result
    except json.JSONDecodeError:
        pass
    m = re.search(r"```(?:json)?\s*(\[.*?])\s*```", text, re.DOTALL)
    if m:
        try:
            return json.loads(m.group(1))
        except json.JSONDecodeError:
            pass
    m = re.search(r"\[.*]", text, re.DOTALL)
    if m:
        try:
            return json.loads(m.group(0))
        except json.JSONDecodeError:
            pass
    return []


class TypedObserver:
    """Extract typed memories from conversation turns.

    Flow: LLM extraction → embed → contradiction detection → store.
    """

    def __init__(
        self,
        store: MemoryStore,
        llm_client: Any = None,
        embed_fn: Any = None,
        contradiction_threshold: float = 0.85,
    ):
        self.store = store
        self.llm = llm_client
        self.embed_fn = embed_fn
        self.contradiction_threshold = contradiction_threshold

    def observe(
        self,
        user_id: str,
        messages: list[dict[str, Any]],
        source_event_ids: Optional[list[str]] = None,
    ) -> list[Memory]:
        """Extract and persist typed memories from a conversation turn."""
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

            stored = self._store_with_contradiction_check(mem)
            results.append(stored)

        return results

    def observe_explicit(
        self,
        user_id: str,
        content: str,
        memory_type: MemoryType,
        confidence: float = 0.9,
        source_event_ids: Optional[list[str]] = None,
    ) -> Memory:
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

        return self._store_with_contradiction_check(mem)

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

    def _store_with_contradiction_check(self, mem: Memory) -> Memory:
        """Check for contradicting memory and supersede if found, else create."""
        if mem.embedding is not None:
            existing = self.store.list_active(mem.user_id, mem.memory_type)
            contradiction = self._find_contradiction(mem, existing)
            if contradiction:
                logger.info(
                    "Contradiction detected: '%s' supersedes '%s'",
                    mem.content[:60], contradiction.content[:60],
                )
                return self.store.supersede(contradiction.memory_id, mem)

        return self.store.create(mem)

    def _find_contradiction(self, new: Memory, existing: list[Memory]) -> Optional[Memory]:
        """Find an existing memory that contradicts the new one.

        Heuristic: same type + high vector similarity + different content.
        """
        if new.embedding is None:
            return None

        best_sim = 0.0
        best_match: Optional[Memory] = None

        for old in existing:
            if old.embedding is None:
                continue
            sim = self._cosine_similarity(new.embedding, old.embedding)
            if sim > self.contradiction_threshold and sim > best_sim:
                if old.content.strip() != new.content.strip():
                    best_sim = sim
                    best_match = old

        return best_match

    @staticmethod
    def _cosine_similarity(a: list[float], b: list[float]) -> float:
        dot = sum(x * y for x, y in zip(a, b))
        norm_a = sum(x * x for x in a) ** 0.5
        norm_b = sum(x * x for x in b) ** 0.5
        if norm_a == 0 or norm_b == 0:
            return 0.0
        return dot / (norm_a * norm_b)
