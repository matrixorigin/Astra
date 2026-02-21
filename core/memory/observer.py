"""Observer agent — extracts structured observations from conversation events.

Runs after each conversation turn (post-chain hook). Converts raw messages
into dense, dated, prioritized observations. Once messages are observed,
they can be dropped from the context window and replaced by observations.

Design: Mastra-inspired observational memory adapted for our cognitive architecture.
- LLM-based extraction (not regex)
- Temporal anchoring (observed_at + referenced_at)
- Priority tagging (high/medium/low)
- Tracks observed message count in DB (survives restart, multi-instance safe)
"""

from __future__ import annotations

import json
import logging
import re
import uuid
from datetime import datetime
from typing import Any

from sqlalchemy.orm import Session

logger = logging.getLogger(__name__)

DEFAULT_OBSERVE_THRESHOLD = 2000

OBSERVER_SYSTEM_PROMPT = """\
You are a memory observer. Your job is to watch a conversation between a user \
and an AI assistant, and extract structured observations.

Rules:
- Each observation captures ONE specific fact, preference, decision, or action.
- Tag priority: "high" (critical decisions, explicit preferences), \
"medium" (useful context), "low" (minor details).
- Tag type: "preference", "decision", "fact", "action", "pattern".
- If the user mentions a specific date or time, extract it as referenced_at (ISO 8601).
- Be dense: compress, don't summarize. Preserve specific names, numbers, dates.
- Do NOT include greetings, filler, or meta-commentary.

Output a JSON array ONLY, no other text:
[
  {
    "content": "User prefers TypeScript over Python for frontend work",
    "priority": "high",
    "type": "preference",
    "referenced_at": null
  }
]

If nothing worth observing, return [].
"""


def _parse_json_array(text: str) -> list[dict[str, Any]]:
    """Robustly extract a JSON array from LLM output.

    Handles: bare JSON, ```json blocks, leading/trailing garbage.
    """
    # Try direct parse first
    text = text.strip()
    try:
        result = json.loads(text)
        if isinstance(result, list):
            return result
    except json.JSONDecodeError:
        pass

    # Try extracting from code block
    m = re.search(r"```(?:json)?\s*(\[.*?])\s*```", text, re.DOTALL)
    if m:
        try:
            return json.loads(m.group(1))
        except json.JSONDecodeError:
            pass

    # Try finding first [ ... ] in the text
    m = re.search(r"\[.*]", text, re.DOTALL)
    if m:
        try:
            return json.loads(m.group(0))
        except json.JSONDecodeError:
            pass

    return []


class Observer:
    """Extract structured observations from conversation events.

    Tracks observed message count per (user, session) in the observations
    table itself (max source index), so it survives restarts and works
    across multiple instances.
    """

    def __init__(self, db: Session, llm_client=None):
        self.db = db
        self.llm = llm_client

    def observe(
        self,
        session_id: str,
        user_id: str,
        messages: list[dict[str, Any]],
        threshold: int = DEFAULT_OBSERVE_THRESHOLD,
    ) -> list[dict[str, Any]]:
        """Extract observations from new (unobserved) messages only.

        Args:
            session_id: Current session
            user_id: User who owns the conversation
            messages: Full message chain (role/content)
            threshold: Token threshold on *unobserved* messages to trigger

        Returns:
            List of created observation records
        """
        from core.context.compaction import estimate_tokens

        observed_idx = self._get_observed_index(session_id)
        new_messages = messages[observed_idx:]

        if not new_messages:
            return []

        token_count = estimate_tokens(new_messages)
        if token_count < threshold:
            return []

        if not self.llm:
            logger.debug("Observer: no LLM client, skipping")
            return []

        raw = self._extract_via_llm(new_messages)

        # Always advance the index, even if extraction returned nothing
        new_idx = len(messages)

        if not raw:
            # Store a sentinel so index advances in DB
            self._advance_index(session_id, user_id, new_idx)
            return []

        return self._store_observations(raw, session_id, user_id, new_messages, new_idx)

    # ------------------------------------------------------------------
    # Observed index: DB-backed, multi-instance safe
    # ------------------------------------------------------------------

    def _get_observed_index(self, session_id: str) -> int:
        """Get the number of messages already observed for this session.

        Uses the max observed_msg_index stored in the observations table.
        Falls back to 0 if no observations exist.
        """
        from api.models import Observation
        from sqlalchemy import func

        result = self.db.query(func.max(Observation.observed_msg_index)).filter(
            Observation.session_id == session_id,
        ).scalar()
        return result or 0

    def _advance_index(self, session_id: str, user_id: str, new_idx: int) -> None:
        """Store a marker observation to advance the observed index.

        Without this, LLM returning [] would cause the same messages to be
        re-processed on every turn — wasting LLM calls.
        """
        from api.models import Observation

        marker = Observation(
            observation_id=str(uuid.uuid4()),
            user_id=user_id,
            session_id=session_id,
            content="[no observations extracted]",
            priority="low",
            observation_type="marker",
            observed_at=datetime.now(),
            source_event_ids="[]",
            is_reflected=1,  # Already "reflected" so it never shows in context
            observed_msg_index=new_idx,
            confidence=0.0,
        )
        self.db.add(marker)
        self.db.commit()

    def get_observed_index(self, session_id: str) -> int:
        """Public accessor for observed index."""
        return self._get_observed_index(session_id)

    # ------------------------------------------------------------------
    # LLM extraction
    # ------------------------------------------------------------------

    def _extract_via_llm(self, messages: list[dict[str, Any]]) -> list[dict[str, Any]]:
        """Call LLM to extract observations from messages."""
        conv_text = "\n".join(
            f"[{m.get('role', 'unknown')}]: {m.get('content', '')[:500]}"
            for m in messages
            if m.get("content")
        )

        try:
            result = self.llm.chat_with_tools(
                messages=[
                    {"role": "system", "content": OBSERVER_SYSTEM_PROMPT},
                    {"role": "user", "content": conv_text},
                ],
                tools=[],
                tool_choice="none",
            )
            return _parse_json_array(result.get("content", ""))
        except Exception as e:
            logger.warning(f"Observer LLM extraction failed: {e}")
            return []

    # ------------------------------------------------------------------
    # Storage
    # ------------------------------------------------------------------

    def _store_observations(
        self,
        raw_observations: list[dict[str, Any]],
        session_id: str,
        user_id: str,
        source_messages: list[dict[str, Any]],
        observed_msg_index: int,
    ) -> list[dict[str, Any]]:
        """Persist observations to DB in a single transaction.

        Deduplicates against existing observations in the same session
        (exact content match). Assigns confidence from priority.
        """
        from api.models import Observation

        source_ids = [
            m.get("event_id", "")
            for m in source_messages
            if m.get("event_id")
        ]

        # Load existing content for dedup
        try:
            existing = set(
                row[0] for row in
                self.db.query(Observation.content).filter(
                    Observation.session_id == session_id,
                    Observation.observation_type != "marker",
                ).all()
            )
        except Exception:
            existing = set()

        priority_confidence = {"high": 0.95, "medium": 0.75, "low": 0.5}
        now = datetime.now()
        stored = []

        for obs in raw_observations:
            if not isinstance(obs, dict) or not obs.get("content"):
                continue

            content = obs["content"]
            if content in existing:
                logger.debug("Observer: skipping duplicate observation: %s", content[:80])
                continue
            existing.add(content)

            ref_at = None
            if obs.get("referenced_at"):
                try:
                    ref_at = datetime.fromisoformat(str(obs["referenced_at"]))
                except (ValueError, TypeError):
                    pass

            priority = obs.get("priority", "medium")
            confidence = priority_confidence.get(priority, 0.75)

            entry = Observation(
                observation_id=str(uuid.uuid4()),
                user_id=user_id,
                session_id=session_id,
                content=content,
                priority=priority,
                observation_type=obs.get("type", "fact"),
                observed_at=now,
                referenced_at=ref_at,
                source_event_ids=json.dumps(source_ids),
                observed_msg_index=observed_msg_index,
                confidence=confidence,
            )
            self.db.add(entry)
            stored.append({
                "observation_id": entry.observation_id,
                "content": entry.content,
                "priority": entry.priority,
                "confidence": confidence,
            })

        if stored:
            self.db.commit()
            logger.info(f"Observer: created {len(stored)} observations for session {session_id}")

        return stored

    # ------------------------------------------------------------------
    # Retrieval
    # ------------------------------------------------------------------

    def get_observations(
        self,
        user_id: str,
        session_id: str | None = None,
        include_cross_session: bool = True,
        limit: int = 100,
    ) -> list[dict[str, Any]]:
        """Retrieve active (non-reflected) observations for context assembly."""
        from api.models import Observation

        q = self.db.query(Observation).filter(
            Observation.user_id == user_id,
            Observation.is_reflected == 0,
        )

        if session_id and not include_cross_session:
            q = q.filter(Observation.session_id == session_id)

        rows = q.order_by(Observation.observed_at.desc()).limit(limit).all()

        return [
            {
                "observation_id": r.observation_id,
                "content": r.content,
                "priority": r.priority,
                "type": r.observation_type,
                "observed_at": r.observed_at.isoformat() if r.observed_at else None,
                "referenced_at": r.referenced_at.isoformat() if r.referenced_at else None,
                "session_id": r.session_id,
                "is_reflected": bool(r.is_reflected),
                "confidence": getattr(r, "confidence", 0.75),
            }
            for r in reversed(rows)  # chronological order
        ]

    # ------------------------------------------------------------------
    # Context assembly
    # ------------------------------------------------------------------

    def format_for_context(self, observations: list[dict[str, Any]]) -> str:
        """Format observations as a prompt section (emoji priorities + dates)."""
        if not observations:
            return ""

        priority_emoji = {"high": "🔴", "medium": "🟡", "low": "🟢"}
        lines = ["## Memory (Observations)"]

        for obs in observations:
            emoji = priority_emoji.get(obs.get("priority", "medium"), "🟡")
            date = obs.get("observed_at", "")[:10] if obs.get("observed_at") else ""
            lines.append(f"- {emoji} [{date}] {obs['content']}")

        return "\n".join(lines)

    def build_context_with_observations(
        self,
        messages: list[dict[str, Any]],
        user_id: str,
        session_id: str,
        preserve_recent: int = 4,
        _cached_obs_section: str | None = None,
    ) -> list[dict[str, Any]]:
        """Replace observed messages with observations in the context window.

        Args:
            messages: Full message chain
            user_id: User
            session_id: Session
            preserve_recent: Keep this many recent messages verbatim
            _cached_obs_section: Pre-formatted observations (avoids repeated DB queries in tool loop)

        Returns:
            New message list: [system + observations] + [recent messages]
        """
        if _cached_obs_section is None:
            observations = self.get_observations(user_id, session_id)
            if not observations:
                return messages
            obs_section = self.format_for_context(observations)
        else:
            obs_section = _cached_obs_section

        if not obs_section:
            return messages

        observed_idx = self._get_observed_index(session_id)
        if observed_idx == 0:
            return messages  # Nothing observed yet

        result = []

        # System message: inject observations
        if messages and messages[0].get("role") == "system":
            system_msg = messages[0].copy()
            # Avoid duplicating observations if already injected
            if "## Memory (Observations)" not in system_msg["content"]:
                system_msg["content"] = system_msg["content"] + "\n\n" + obs_section
            result.append(system_msg)
            remaining = messages[1:]
        else:
            result.append({"role": "system", "content": obs_section})
            remaining = messages

        # Drop observed messages, keep recent unobserved ones
        # observed_idx is relative to full messages array (including system)
        has_system = bool(messages and messages[0].get("role") == "system")
        adj_idx = max(0, observed_idx - (1 if has_system else 0))
        recent = remaining[adj_idx:]

        # Always keep at least preserve_recent messages
        if len(recent) < preserve_recent and len(remaining) >= preserve_recent:
            recent = remaining[-preserve_recent:]
        elif len(recent) < preserve_recent:
            recent = remaining

        result.extend(recent)
        return result
