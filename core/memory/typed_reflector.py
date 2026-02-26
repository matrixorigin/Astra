"""Typed Reflector — episodic→semantic promotion and procedural detection.

Works with the new Memory model (not Observation).
"""

from __future__ import annotations

import json
import logging
import uuid
from datetime import datetime
from typing import Any, Optional

from core.memory.prompts import REFLECTOR_CONDENSATION_PROMPT
from core.memory.store import MemoryStore
from core.memory.types import Memory, MemoryType

logger = logging.getLogger(__name__)


class TypedReflector:
    """Promote episodic memories to semantic, detect procedural patterns."""

    def __init__(
        self,
        store: MemoryStore,
        llm_client: Any = None,
        embed_fn: Any = None,
        cluster_similarity: float = 0.7,
        cluster_min_size: int = 3,
    ):
        self.store = store
        self.llm = llm_client
        self.embed_fn = embed_fn
        self.cluster_similarity = cluster_similarity
        self.cluster_min_size = cluster_min_size

    def reflect(self, user_id: str) -> dict[str, int]:
        """Run reflection: episodic→semantic promotion.

        Returns: {"promoted": N, "clusters_found": M}
        """
        episodics = self.store.list_active(user_id, MemoryType.EPISODIC)
        if len(episodics) < self.cluster_min_size:
            return {"promoted": 0, "clusters_found": 0}

        clusters = self._find_clusters(episodics)
        if not clusters:
            return {"promoted": 0, "clusters_found": 0}

        promoted = 0
        for cluster in clusters:
            semantic = self._condense_cluster(user_id, cluster)
            if semantic:
                promoted += 1

        return {"promoted": promoted, "clusters_found": len(clusters)}

    def _find_clusters(self, memories: list[Memory]) -> list[list[Memory]]:
        """Find clusters of similar episodic memories."""
        if not memories:
            return []

        # Filter to those with embeddings
        with_emb = [m for m in memories if m.embedding is not None]
        if len(with_emb) < self.cluster_min_size:
            return []

        used = set()
        clusters = []

        for i, m1 in enumerate(with_emb):
            if m1.memory_id in used:
                continue

            cluster = [m1]
            for j, m2 in enumerate(with_emb):
                if i == j or m2.memory_id in used:
                    continue
                sim = self._cosine_similarity(m1.embedding, m2.embedding)
                if sim >= self.cluster_similarity:
                    cluster.append(m2)

            if len(cluster) >= self.cluster_min_size:
                clusters.append(cluster)
                for m in cluster:
                    used.add(m.memory_id)

        return clusters

    def _condense_cluster(self, user_id: str, cluster: list[Memory]) -> Optional[Memory]:
        """Condense a cluster of episodics into one semantic memory."""
        if not self.llm:
            return None

        episodic_list = "\n".join(f"- {m.content}" for m in cluster)
        prompt = REFLECTOR_CONDENSATION_PROMPT.format(episodic_list=episodic_list)

        try:
            result = self.llm.chat_with_tools(
                messages=[{"role": "user", "content": prompt}],
                tools=[], tool_choice="none",
            )
            parsed = self._parse_json(result.get("content", ""))
            if not parsed or not parsed.get("content"):
                return None

            avg_conf = sum(m.confidence for m in cluster) / len(cluster)
            semantic = Memory(
                memory_id=uuid.uuid4().hex,
                user_id=user_id,
                memory_type=MemoryType.SEMANTIC,
                content=parsed["content"],
                confidence=parsed.get("confidence", avg_conf),
                source_event_ids=[m.memory_id for m in cluster],
                observed_at=datetime.utcnow(),
            )

            if self.embed_fn:
                try:
                    semantic.embedding = self.embed_fn(semantic.content)
                except Exception as e:
                    logger.warning("Embedding failed: %s", e)

            # Supersede all cluster members
            for old in cluster:
                self.store.deactivate(old.memory_id)

            self.store.create(semantic)
            logger.info(
                "Reflector: condensed %d episodics → 1 semantic: '%s'",
                len(cluster), semantic.content[:60],
            )
            return semantic

        except Exception as e:
            logger.warning("Reflector condensation failed: %s", e)
            return None

    @staticmethod
    def _parse_json(text: str) -> Optional[dict]:
        text = text.strip()
        try:
            return json.loads(text)
        except json.JSONDecodeError:
            pass
        import re
        m = re.search(r"\{.*\}", text, re.DOTALL)
        if m:
            try:
                return json.loads(m.group(0))
            except json.JSONDecodeError:
                pass
        return None

    @staticmethod
    def _cosine_similarity(a: list[float], b: list[float]) -> float:
        dot = sum(x * y for x, y in zip(a, b))
        norm_a = sum(x * x for x in a) ** 0.5
        norm_b = sum(x * x for x in b) ** 0.5
        if norm_a == 0 or norm_b == 0:
            return 0.0
        return dot / (norm_a * norm_b)
