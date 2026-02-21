"""Dynamic few-shot example retrieval from feedback history.

Retrieves high-rated input→output pairs similar to the current query
and formats them for injection into the system prompt.
"""

import json
import logging
from typing import Any

from sqlalchemy import text
from sqlalchemy.orm import Session

logger = logging.getLogger(__name__)


class FewShotRetriever:
    """Retrieve high-quality examples from llm_feedback for few-shot prompting."""

    def __init__(self, db: Session, min_rating: int = 4, max_examples: int = 2):
        self.db = db
        self.min_rating = min_rating
        self.max_examples = max_examples

    def retrieve(self, query: str, task_type: str | None = None) -> list[dict[str, Any]]:
        """Find similar high-rated examples via conversation_events.

        Strategy: feedback → context_snapshot → event → find user query + next agent response.
        Falls back gracefully if no feedback data exists.
        """
        try:
            sql = """
                SELECT f.rating, e_user.content AS user_query, e_agent.content AS agent_response
                FROM llm_feedback f
                JOIN context_snapshots cs ON f.llm_request_id = cs.llm_request_id
                JOIN conversation_events e_user ON cs.event_id = e_user.event_id
                LEFT JOIN conversation_events e_agent
                    ON e_agent.parent_event_id = e_user.event_id
                    AND e_agent.event_type = 'llm_response'
                WHERE f.rating >= :min_rating AND e_user.content IS NOT NULL
                ORDER BY f.rating DESC, f.created_at DESC
                LIMIT :limit
            """
            rows = self.db.execute(
                text(sql), {"min_rating": self.min_rating, "limit": self.max_examples * 5}
            ).fetchall()

            if not rows:
                return []

            # Score by keyword overlap with current query
            query_words = set(query.lower().split())
            scored = []
            for row in rows:
                req = (row[1] or "").lower()
                overlap = len(query_words & set(req.split()))
                if overlap > 0:
                    scored.append((overlap, row))

            scored.sort(key=lambda x: (-x[0], -x[1][0]))  # overlap desc, rating desc

            examples = []
            for _, row in scored[: self.max_examples]:
                examples.append({
                    "input": row[1],
                    "output": row[2] or "(no response captured)",
                    "rating": row[0],
                })
            return examples

        except Exception as e:
            logger.debug(f"Few-shot retrieval unavailable: {e}")
            return []

    def format_for_prompt(self, examples: list[dict[str, Any]]) -> str:
        """Format examples as a prompt section."""
        if not examples:
            return ""
        parts = ["Examples of good responses:"]
        for i, ex in enumerate(examples, 1):
            parts.append(f"\nExample {i}:")
            parts.append(f"User: {ex['input']}")
            # Truncate long outputs
            output = ex["output"] or ""
            if len(output) > 500:
                output = output[:500] + "..."
            parts.append(f"Assistant: {output}")
        return "\n".join(parts)
