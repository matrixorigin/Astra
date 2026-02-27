"""Semantic Diff - Compare agent decisions and behaviors.

Provides high-level comparison of agent performance, not just data differences.
Includes content-level semantic similarity via embeddings.
"""

import re
from collections import Counter

from core.events.event_reader import EventReader
from core.db_consumer import DbConsumer, DbFactory
from core.context.embeddings import EmbeddingService, get_embedding_client
from core.skills.learning_similarity import cosine_similarity

_SAFE_NAME_RE = re.compile(r"^[a-zA-Z0-9_-]+$")

_EVENT_COLUMNS = (
    "event_id, user_id, session_id, agent_id, agent_version, "
    "event_type, content, metadata, created_at, parent_event_id, causal_chain_id"
)


def _validate_name(name: str, label: str = "name") -> None:
    """Validate that name contains only safe characters."""
    if not _SAFE_NAME_RE.match(name):
        raise ValueError(
            f"Invalid {label}: {name!r}. Only alphanumeric, dash, underscore allowed."
        )


class SemanticDiff(DbConsumer):
    """Semantic difference analyzer for agent behaviors."""

    def __init__(self, db_factory: DbFactory, embedding_service: EmbeddingService | None = None) -> None:
        super().__init__(db_factory)
        self.reader = EventReader(self._db_factory)
        self._embedder = embedding_service or EmbeddingService(db_factory)

    def compare_sessions(self, session_id1: str, session_id2: str) -> dict:
        """Compare two sessions semantically."""
        events1 = self.reader.get_session_events(session_id1)
        events2 = self.reader.get_session_events(session_id2)

        token_diff = self._compare_token_usage(events1, events2)
        path_diff = self._compare_decision_paths(events1, events2)
        type_diff = self._compare_event_types(events1, events2)
        quality_diff = self._compare_quality(events1, events2)
        content_diff = self._compare_content_similarity(events1, events2)

        return {
            "session1": session_id1,
            "session2": session_id2,
            "token_usage": token_diff,
            "decision_paths": path_diff,
            "event_types": type_diff,
            "quality": quality_diff,
            "content_similarity": content_diff,
            "summary": self._generate_summary(token_diff, path_diff, type_diff, content_diff),
        }

    def compare_checkpoints(self, checkpoint1: str, checkpoint2: str, session_id: str) -> dict:
        """Compare agent behavior at two different checkpoints.

        Checkpoint names are validated against injection before use in SQL.
        MatrixOne SNAPSHOT syntax does not support parameterized names.
        """
        _validate_name(checkpoint1, "checkpoint1")
        _validate_name(checkpoint2, "checkpoint2")

        with self._db() as db:
            from sqlalchemy import text

            query1 = text(f"""
                SELECT {_EVENT_COLUMNS}
                FROM conversation_events {{SNAPSHOT = '{checkpoint1}'}}
                WHERE session_id = :session_id
                ORDER BY created_at ASC
            """)
            events1_rows = db.execute(query1, {"session_id": session_id}).fetchall()
            events1 = [self.reader._row_to_event(dict(row._mapping)) for row in events1_rows]

            query2 = text(f"""
                SELECT {_EVENT_COLUMNS}
                FROM conversation_events {{SNAPSHOT = '{checkpoint2}'}}
                WHERE session_id = :session_id
                ORDER BY created_at ASC
            """)
            events2_rows = db.execute(query2, {"session_id": session_id}).fetchall()
            events2 = [self.reader._row_to_event(dict(row._mapping)) for row in events2_rows]

            token_diff = self._compare_token_usage(events1, events2)
            path_diff = self._compare_decision_paths(events1, events2)

            return {
                "checkpoint1": checkpoint1,
                "checkpoint2": checkpoint2,
                "session_id": session_id,
                "token_usage": token_diff,
                "decision_paths": path_diff,
                "event_count_diff": len(events2) - len(events1),
            }

    @staticmethod
    def _compare_token_usage(events1: list, events2: list) -> dict:
        """Compare token usage between two event lists."""
        total1 = sum(e.token_usage.total for e in events1 if e.token_usage)
        total2 = sum(e.token_usage.total for e in events2 if e.token_usage)
        prompt1 = sum(e.token_usage.prompt for e in events1 if e.token_usage)
        prompt2 = sum(e.token_usage.prompt for e in events2 if e.token_usage)
        completion1 = sum(e.token_usage.completion for e in events1 if e.token_usage)
        completion2 = sum(e.token_usage.completion for e in events2 if e.token_usage)

        return {
            "total": {"session1": total1, "session2": total2, "diff": total2 - total1},
            "prompt": {"session1": prompt1, "session2": prompt2, "diff": prompt2 - prompt1},
            "completion": {"session1": completion1, "session2": completion2, "diff": completion2 - completion1},
            "efficiency_change": f"{((total2 - total1) / total1 * 100):.1f}%" if total1 > 0 else "N/A",
        }

    @staticmethod
    def _compare_decision_paths(events1: list, events2: list) -> dict:
        """Compare decision paths (causal chains)."""
        chains1 = {e.causal_chain_id for e in events1 if e.causal_chain_id}
        chains2 = {e.causal_chain_id for e in events2 if e.causal_chain_id}

        def _avg_chain_len(events, chains):
            if not chains:
                return 0
            lengths = {c: sum(1 for e in events if e.causal_chain_id == c) for c in chains}
            return sum(lengths.values()) / len(lengths)

        avg1 = _avg_chain_len(events1, chains1)
        avg2 = _avg_chain_len(events2, chains2)

        return {
            "chain_count": {"session1": len(chains1), "session2": len(chains2), "diff": len(chains2) - len(chains1)},
            "avg_chain_length": {"session1": avg1, "session2": avg2, "diff": avg2 - avg1},
            "complexity_change": "increased" if avg2 > avg1 else "decreased",
        }

    @staticmethod
    def _compare_event_types(events1: list, events2: list) -> dict:
        """Compare event type distribution."""
        types1 = Counter(e.event_type for e in events1)
        types2 = Counter(e.event_type for e in events2)
        all_types = set(types1.keys()) | set(types2.keys())
        return {
            t: {"session1": types1.get(t, 0), "session2": types2.get(t, 0), "diff": types2.get(t, 0) - types1.get(t, 0)}
            for t in all_types
        }

    @staticmethod
    def _compare_quality(events1: list, events2: list) -> dict:
        """Compare response quality scores."""
        scores1 = [e.quality_score for e in events1 if e.quality_score is not None]
        scores2 = [e.quality_score for e in events2 if e.quality_score is not None]
        avg1 = sum(scores1) / len(scores1) if scores1 else 0
        avg2 = sum(scores2) / len(scores2) if scores2 else 0
        return {
            "avg_quality": {"session1": avg1, "session2": avg2, "diff": avg2 - avg1},
            "quality_change": "improved" if avg2 > avg1 else "degraded",
        }

    @staticmethod
    def _generate_summary(token_diff: dict, path_diff: dict, type_diff: dict, content_diff: dict | None = None) -> str:
        """Generate human-readable summary."""
        parts = []
        tc = token_diff["total"]["diff"]
        if tc > 0:
            parts.append(f"Used {tc} more tokens")
        elif tc < 0:
            parts.append(f"Saved {abs(tc)} tokens")

        cc = path_diff["chain_count"]["diff"]
        if cc > 0:
            parts.append(f"{cc} more decision chains")
        elif cc < 0:
            parts.append(f"{abs(cc)} fewer decision chains")

        if content_diff and content_diff.get("overall") is not None:
            sim = content_diff["overall"]
            if sim < 0.7:
                parts.append(f"Content similarity LOW ({sim:.2f})")
            elif sim < 0.9:
                parts.append(f"Content similarity moderate ({sim:.2f})")

        return "; ".join(parts) if parts else "No significant changes"

    def _compare_content_similarity(self, events1: list, events2: list) -> dict:
        """Compare LLM response content via embedding cosine similarity.

        Pairs LLM responses by position and computes per-pair similarity.
        Overall score is the mean. Low similarity flags semantic regression.
        """
        responses1 = [e.content for e in events1 if e.event_type == "llm_response" and e.content]
        responses2 = [e.content for e in events2 if e.event_type == "llm_response" and e.content]

        if not responses1 or not responses2:
            return {"overall": None, "pairs": [], "note": "No LLM responses to compare"}

        pairs = []
        for i, (r1, r2) in enumerate(zip(responses1, responses2)):
            v1 = self._embedder.embed_text(r1)
            v2 = self._embedder.embed_text(r2)
            sim = cosine_similarity(v1, v2)
            pairs.append({
                "index": i,
                "similarity": round(sim, 4),
                "preview1": r1[:80],
                "preview2": r2[:80],
            })

        overall = sum(p["similarity"] for p in pairs) / len(pairs)
        return {
            "overall": round(overall, 4),
            "pairs": pairs,
            "responses_compared": len(pairs),
            "responses_session1": len(responses1),
            "responses_session2": len(responses2),
        }
