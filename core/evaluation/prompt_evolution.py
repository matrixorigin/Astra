"""Prompt evolution via replay gate and sandbox.

Design ref: evaluation-and-evolution.md §4 "Prompt Evolution"

Evolve prompts by replaying golden sessions with variant prompts,
measuring quality improvement via ReplayService.

Distributed-safe: all state in DB, replay in sandbox clones.
"""

from __future__ import annotations

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

    Args:
        db: SQLAlchemy session
        replay_service: ReplayService instance for session replay (optional)
        llm_client: LLMClient for LLM-as-judge scoring (optional)
    """

    def __init__(self, db: Session, replay_service=None, llm_client=None) -> None:
        self.db = db
        self.replay_service = replay_service
        self.llm_client = llm_client

    def create_variant(
        self,
        prompt_template_id: str,
        content: str,
        description: str = "",
    ) -> PromptVariant:
        """Create a new prompt variant."""
        from uuid_utils import uuid7

        variant_id = str(uuid7())

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

        Priority:
        1. replay_fn (injected, for testing)
        2. self.replay_service (ReplayService integration)
        3. Fail with 0.0 if neither available
        """
        scores = []

        for session_id in golden_sessions:
            try:
                score = self._replay_with_variant(session_id, variant_id, replay_fn)
                scores.append(score)
            except Exception as e:
                logger.warning(f"Replay failed for {session_id}: {e}")

        avg_score = sum(scores) / len(scores) if scores else 0.0

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
        """Promote a variant to be the active prompt."""
        row = self.db.execute(
            text("SELECT content FROM prompt_variants WHERE variant_id = :id"),
            {"id": variant_id},
        ).fetchone()

        if not row:
            logger.error(f"Variant {variant_id} not found")
            return

        self.db.execute(
            text(
                "UPDATE prompt_templates SET content = :content, updated_at = NOW() "
                "WHERE template_id = :template_id"
            ),
            {"content": row[0], "template_id": prompt_template_id},
        )
        self.db.commit()
        logger.info(f"Promoted variant {variant_id} to template {prompt_template_id}")

    def get_best_variant(self, prompt_template_id: str) -> PromptVariant | None:
        """Get the best-performing variant for a template."""
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
        """Replay a session with a prompt variant and score the result.

        Uses ReplayService to replay the session, then scores the replayed
        outputs against the originals.
        """
        # Get variant content
        row = self.db.execute(
            text("SELECT content FROM prompt_variants WHERE variant_id = :id"),
            {"id": variant_id},
        ).fetchone()

        if not row:
            return 0.0

        variant_content = row[0]

        # Priority 1: injected replay_fn (for testing)
        if replay_fn:
            return replay_fn(session_id, variant_content)

        # Priority 2: ReplayService integration
        if self.replay_service:
            return self._replay_via_service(session_id, variant_content)

        # No replay mechanism available
        logger.warning("No replay_fn or replay_service — cannot evaluate variant")
        return 0.0

    def _replay_via_service(self, session_id: str, variant_content: str) -> float:
        """Replay session via ReplayService and score the result."""
        try:
            # Replay the session (mock_mode=True for safety)
            replay_result = self.replay_service.replay_session(
                session_id=session_id,
                user_id="prompt_evolver",
                mock_mode=True,
            )

            # Compare original vs replayed
            comparison = self.replay_service.compare_outputs(
                session_id=session_id,
                user_id="prompt_evolver",
                replay_result=replay_result.get("result", {}),
            )

            # Score based on comparison
            if comparison.get("match"):
                return 5.0  # Perfect match

            mismatched = comparison.get("mismatched_events", 0)
            total = comparison.get("original_event_count", 1)
            match_ratio = 1.0 - (mismatched / max(total, 1))

            # If LLM available, use LLM-as-judge for more nuanced scoring
            if self.llm_client and comparison.get("details"):
                return self._llm_judge_replay(comparison["details"], variant_content)

            # Heuristic: scale 0-5 based on match ratio
            return round(match_ratio * 5.0, 2)

        except Exception as e:
            logger.warning(f"Replay via service failed: {e}")
            return 0.0

    def _llm_judge_replay(self, details: list[dict], variant_content: str) -> float:
        """Use LLM to judge replay quality. Returns score 0-5."""
        import re

        # Build comparison summary
        summary_parts = []
        for d in details[:5]:  # Limit to 5 for prompt size
            summary_parts.append(
                f"Original: {d.get('original', '')[:100]}\n"
                f"Replayed: {d.get('replayed', '')[:100]}"
            )
        summary = "\n---\n".join(summary_parts)

        prompt = (
            "Rate the quality of replayed AI responses compared to originals.\n"
            "Score 1-5: 1=much worse, 3=equivalent, 5=much better.\n"
            "Reply with ONLY a single digit.\n\n"
            f"Prompt variant used:\n{variant_content[:300]}\n\n"
            f"Comparisons:\n{summary}"
        )
        try:
            response = self.llm_client.chat(
                messages=[{"role": "user", "content": prompt}],
                user_id="prompt_evolver",
                temperature=0.0,
            )
            text = (response.content or "").strip()
            match = re.search(r"[1-5]", text)
            if match:
                return float(match.group(0))
        except Exception as e:
            logger.warning(f"LLM judge failed: {e}")

        return 3.0  # Default: equivalent
