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
        """Find similar high-rated examples.

        Strategy: keyword overlap between query and stored request content.
        Falls back gracefully if no feedback data exists.
        """
        try:
            # Get high-rated feedback with linked LLM request content
            sql = """
                SELECT f.llm_request_id, f.rating, f.prompt_template_id,
                       l.request_content, l.response_content
                FROM llm_feedback f
                JOIN llm_call_logs l ON f.llm_request_id = l.event_id
                WHERE f.rating >= :min_rating
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
                req = (row[3] or "").lower()
                overlap = len(query_words & set(req.split()))
                if overlap > 0:
                    scored.append((overlap, row))

            scored.sort(key=lambda x: (-x[0], -x[1][1]))  # overlap desc, rating desc

            examples = []
            for _, row in scored[: self.max_examples]:
                examples.append({
                    "input": row[3],
                    "output": row[4],
                    "rating": row[1],
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
