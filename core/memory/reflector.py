"""Reflector agent — restructures and condenses accumulated observations.

When observations exceed a token threshold, the Reflector:
1. Combines related observations
2. Identifies overarching patterns
3. Drops superseded/redundant observations
4. Produces a condensed observation set

Transaction-safe: mark old + insert new in a single commit.
"""

from __future__ import annotations

import json
import logging
import uuid
from datetime import datetime
from typing import Any

from sqlalchemy.orm import Session

logger = logging.getLogger(__name__)

DEFAULT_REFLECT_THRESHOLD = 8000

REFLECTOR_SYSTEM_PROMPT = """\
You are a memory reflector. You receive a list of observations about a user's \
conversations with an AI assistant. Your job is to restructure and condense them.

Rules:
- Combine related observations into single, denser observations.
- Identify overarching patterns (e.g. "User consistently prefers X").
- Drop observations that have been superseded by newer ones.
- Preserve all dates, names, numbers — never lose specific facts.
- Keep priority tags. Promote patterns to "high" priority.
- Tag new pattern observations as type "pattern".

Input: JSON array of observations (each has content, priority, type, observed_at).
Output a JSON array ONLY, no other text. The output should be significantly \
shorter than the input while preserving all important information.
"""


class Reflector:
    """Restructure and condense accumulated observations."""

    def __init__(self, db: Session, llm_client=None):
        self.db = db
        self.llm = llm_client

    def reflect(
        self,
        user_id: str,
        session_id: str | None = None,
        threshold: int = DEFAULT_REFLECT_THRESHOLD,
    ) -> dict[str, int]:
        """Run reflection if observations exceed threshold.

        Returns:
            {"before": N, "after": M, "reflected": bool}
        """
        from core.context.compaction import estimate_tokens
        from core.memory.observer import Observer

        observer = Observer(self.db)
        observations = observer.get_observations(user_id, session_id)

        if not observations:
            return {"before": 0, "after": 0, "reflected": False}

        obs_text = "\n".join(o["content"] for o in observations)
        tokens = estimate_tokens([{"role": "user", "content": obs_text}])

        if tokens < threshold:
            return {"before": len(observations), "after": len(observations), "reflected": False}

        if not self.llm:
            logger.debug("Reflector: no LLM client, skipping")
            return {"before": len(observations), "after": len(observations), "reflected": False}

        condensed = self._reflect_via_llm(observations)
        if not condensed:
            return {"before": len(observations), "after": len(observations), "reflected": False}

        replaced = self._replace_observations(
            user_id, session_id, observations, condensed,
        )

        return {
            "before": len(observations),
            "after": replaced,
            "reflected": True,
        }

    def _reflect_via_llm(self, observations: list[dict[str, Any]]) -> list[dict[str, Any]]:
        """Call LLM to condense observations."""
        from core.memory.observer import _parse_json_array

        input_json = json.dumps(observations, default=str)

        try:
            result = self.llm.chat_with_tools(
                messages=[
                    {"role": "system", "content": REFLECTOR_SYSTEM_PROMPT},
                    {"role": "user", "content": input_json},
                ],
                tools=[],
                tool_choice="none",
            )
            return _parse_json_array(result.get("content", ""))
        except Exception as e:
            logger.warning(f"Reflector LLM failed: {e}")
            return []

    def _replace_observations(
        self,
        user_id: str,
        session_id: str | None,
        old_observations: list[dict[str, Any]],
        condensed: list[dict[str, Any]],
    ) -> int:
        """Mark old observations as reflected and insert condensed ones.

        Runs in a single transaction: if anything fails, nothing changes.
        """
        from api.models import Observation

        old_ids = [o["observation_id"] for o in old_observations if o.get("observation_id")]

        try:
            # Mark old as reflected
            if old_ids:
                self.db.query(Observation).filter(
                    Observation.observation_id.in_(old_ids)
                ).update(
                    {Observation.is_reflected: 1},
                    synchronize_session=False,
                )

            # Insert condensed
            now = datetime.now()
            count = 0
            for obs in condensed:
                if not isinstance(obs, dict) or not obs.get("content"):
                    continue

                ref_at = None
                if obs.get("referenced_at"):
                    try:
                        ref_at = datetime.fromisoformat(str(obs["referenced_at"]))
                    except (ValueError, TypeError):
                        pass

                entry = Observation(
                    observation_id=str(uuid.uuid4()),
                    user_id=user_id,
                    session_id=session_id or "",
                    content=obs["content"],
                    priority=obs.get("priority", "medium"),
                    observation_type=obs.get("type", "pattern"),
                    observed_at=now,
                    referenced_at=ref_at,
                    source_event_ids=json.dumps(old_ids),
                    version=2,
                )
                self.db.add(entry)
                count += 1

            # Single commit: atomic mark + insert
            self.db.commit()
            logger.info(
                f"Reflector: {len(old_observations)} → {count} observations "
                f"for user {user_id}"
            )
            return count

        except Exception:
            self.db.rollback()
            logger.exception("Reflector transaction failed, rolled back")
            raise
