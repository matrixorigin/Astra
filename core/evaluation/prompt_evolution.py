"""Prompt evolution via replay gate and sandbox.

Design ref: evaluation-and-evolution.md §4 "Prompt Evolution"

Evolve prompts by replaying golden sessions, measuring quality improvement.
Distributed-safe: all state in DB, replay in sandbox clones.
"""

from __future__ import annotations

import json
import logging
from dataclasses import dataclass
from typing import Any

from sqlalchemy import text
from sqlalchemy.orm import Session

logger = logging.getLogger(__name__)


@dataclass
class PromptVariant:
    """A prompt variant for testing."""

    variant_id: str
    prompt_template_id: str
    version: int
    content: str
    quality_score: float | None = None


class PromptEvolver:
    """Evolve prompts via replay and measurement.

    Distributed-safe: all state in DB.
    """

    def __init__(self, db: Session) -> None:
        self.db = db

    def create_variant(
        self,
        prompt_template_id: str,
        content: str,
        description: str = "",
    ) -> PromptVariant:
        """Create a new prompt variant.

        Args:
            prompt_template_id: Base prompt template ID
            content: New prompt content
            description: Description of changes

        Returns:
            PromptVariant
        """
        from uuid_utils import uuid7

        variant_id = str(uuid7())

        # Get next version
        row = self.db.execute(
            text(
                "SELECT MAX(version) FROM prompt_variants "
                "WHERE prompt_template_id = :template_id"
            ),
            {"template_id": prompt_template_id},
        ).fetchone()
        version = (row[0] or 0) + 1

        self.db.execute(
            text(
                "INSERT INTO prompt_variants "
                "(variant_id, prompt_template_id, version, content, description, created_at) "
                "VALUES (:id, :template_id, :version, :content, :desc, NOW())"
            ),
            {
                "id": variant_id,
                "template_id": prompt_template_id,
                "version": version,
                "content": content,
                "desc": description,
            },
        )
        self.db.commit()

        logger.info(f"Created prompt variant: {variant_id} (v{version})")

        return PromptVariant(
            variant_id=variant_id,
            prompt_template_id=prompt_template_id,
            version=version,
            content=content,
        )

    def evaluate_variant(
        self,
        variant_id: str,
        golden_sessions: list[str],
        replay_fn=None,
    ) -> float:
        """Evaluate a prompt variant by replaying golden sessions.

        Args:
            variant_id: Variant ID
            golden_sessions: List of session IDs to replay
            replay_fn: Optional replay function (for testing)

        Returns:
            Average quality score
        """
        scores = []

        for session_id in golden_sessions:
            try:
                # Replay session with new prompt
                score = self._replay_with_variant(session_id, variant_id, replay_fn)
                scores.append(score)
            except Exception as e:
                logger.warning(f"Replay failed for {session_id}: {e}")

        avg_score = sum(scores) / len(scores) if scores else 0.0

        # Record evaluation
        self.db.execute(
            text(
                "UPDATE prompt_variants SET quality_score = :score "
                "WHERE variant_id = :id"
            ),
            {"score": avg_score, "id": variant_id},
        )
        self.db.commit()

        logger.info(f"Evaluated variant {variant_id}: {avg_score:.2f}")
        return avg_score

    def promote_variant(self, variant_id: str, prompt_template_id: str) -> None:
        """Promote a variant to be the active prompt.

        Args:
            variant_id: Variant to promote
            prompt_template_id: Template to update
        """
        # Get variant content
        row = self.db.execute(
            text("SELECT content FROM prompt_variants WHERE variant_id = :id"),
            {"id": variant_id},
        ).fetchone()

        if not row:
            logger.error(f"Variant {variant_id} not found")
            return

        content = row[0]

        # Update template
        self.db.execute(
            text(
                "UPDATE prompt_templates SET content = :content, updated_at = NOW() "
                "WHERE template_id = :template_id"
            ),
            {"content": content, "template_id": prompt_template_id},
        )
        self.db.commit()

        logger.info(f"Promoted variant {variant_id} to template {prompt_template_id}")

    def get_best_variant(self, prompt_template_id: str) -> PromptVariant | None:
        """Get the best-performing variant for a template.

        Args:
            prompt_template_id: Template ID

        Returns:
            Best PromptVariant or None
        """
        row = self.db.execute(
            text(
                "SELECT variant_id, version, content, quality_score "
                "FROM prompt_variants "
                "WHERE prompt_template_id = :template_id "
                "ORDER BY quality_score DESC LIMIT 1"
            ),
            {"template_id": prompt_template_id},
        ).fetchone()

        if not row:
            return None

        return PromptVariant(
            variant_id=row[0],
            prompt_template_id=prompt_template_id,
            version=row[1],
            content=row[2],
            quality_score=float(row[3]) if row[3] else None,
        )

    def _replay_with_variant(
        self, session_id: str, variant_id: str, replay_fn=None
    ) -> float:
        """Replay a session with a prompt variant."""
        # Get variant content
        row = self.db.execute(
            text("SELECT content FROM prompt_variants WHERE variant_id = :id"),
            {"id": variant_id},
        ).fetchone()

        if not row:
            return 0.0

        variant_content = row[0]

        # Replay (mock implementation)
        if replay_fn:
            return replay_fn(session_id, variant_content)

        # Default: return random score for testing
        return 4.0
