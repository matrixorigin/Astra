"""Cross-session continuity: load prior context when a user returns.

Design ref: memory-and-context.md §2 "Cross-Session Continuity"

When a user starts a new session, this module assembles prior context:
1. Session summaries — what happened in recent sessions (episodic)
2. User knowledge — preferences, patterns, facts (semantic)
3. Active notes — unfinished plans/todos from scratchpad (working)
"""

from __future__ import annotations

import json
import logging
from dataclasses import dataclass, field
from datetime import datetime, timezone
from typing import Any

from sqlalchemy import text
from sqlalchemy.orm import Session
from core.db_consumer import DbConsumer, DbFactory

logger = logging.getLogger(__name__)


@dataclass
class PriorContext:
    """Assembled prior context from previous sessions."""

    session_summaries: list[dict[str, Any]] = field(default_factory=list)
    knowledge_entries: list[dict[str, Any]] = field(default_factory=list)
    active_notes: list[dict[str, Any]] = field(default_factory=list)

    def to_prompt_section(self) -> str | None:
        """Render as a prompt section. Returns None if empty."""
        parts: list[str] = []

        if self.session_summaries:
            parts.append("## Previous Sessions")
            for s in self.session_summaries:
                label = s.get("title") or s["session_id"][:8]
                parts.append(f"- [{label}] {s['summary']}")

        if self.knowledge_entries:
            parts.append("## What I Know About You")
            for k in self.knowledge_entries:
                parts.append(f"- {k['key']}: {k['value']}")

        if self.active_notes:
            parts.append("## Unfinished Work")
            for n in self.active_notes:
                parts.append(f"- [{n['note_type']}] {n['content']}")

        return "\n".join(parts) if parts else None


class SessionContinuity(DbConsumer):
    """Load prior context for cross-session continuity.

    Distributed-safe: all queries are read-only SELECTs with LIMIT.
    No locking, no writes — safe for concurrent replicas.
    """

    # Defaults
    MAX_SUMMARIES = 5
    MAX_KNOWLEDGE = 20
    MAX_NOTES = 10

    def __init__(self, db_factory: DbFactory) -> None:
        super().__init__(db_factory)

    def load_prior_context(
        self,
        user_id: str,
        current_session_id: str | None = None,
        max_summaries: int = MAX_SUMMARIES,
        max_knowledge: int = MAX_KNOWLEDGE,
        max_notes: int = MAX_NOTES,
    ) -> PriorContext:
        """Load prior context for a user.

        Args:
            user_id: User identifier
            current_session_id: Exclude this session from summaries
            max_summaries: Max recent session summaries
            max_knowledge: Max knowledge entries (highest confidence first)
            max_notes: Max active scratchpad notes
        """
        return PriorContext(
            session_summaries=self._load_session_summaries(
                user_id, current_session_id, max_summaries,
            ),
            knowledge_entries=self._load_knowledge(user_id, max_knowledge),
            active_notes=self._load_active_notes(user_id, max_notes),
        )

    def summarize_session(
        self,
        session_id: str,
        summary: str,
    ) -> None:
        """Store a session summary after session ends.

        Uses conversation_events with event_type='session_summary' to avoid
        schema migration. The summary is stored as the event content.

        Distributed-safe: INSERT only, no read-modify-write.
        """
        with self._db() as db:
            from core.utils.id_generator import generate_id

            eid = generate_id()
            # INSERT...SELECT: pull session_id + user_id from sessions table,
            # inject system constants for agent_id/agent_version/causal_chain_id
            db.execute(
                text(
                    "INSERT INTO conversation_events "
                    "(event_id, session_id, user_id, agent_id, agent_version, "
                    "event_type, content, causal_chain_id, created_at) "
                    "SELECT :event_id, s.session_id, s.user_id, 'system', '1.0.0', "
                    "'session_summary', :summary, :event_id, NOW() "
                    "FROM sessions s WHERE s.session_id = :session_id"
                ),
                {
                    "event_id": eid,
                    "session_id": session_id,
                    "summary": summary,
                },
            )
            db.commit()
            logger.info(f"Session summary stored for {session_id}")

    def _load_session_summaries(
        self, user_id: str, exclude_session: str | None, limit: int,
    ) -> list[dict[str, Any]]:
        """Load recent session summaries from events."""
        with self._db() as db:
            params: dict[str, Any] = {"user_id": user_id, "limit": limit}
            exclude_clause = ""
            if exclude_session:
                exclude_clause = "AND e.session_id != :exclude"
                params["exclude"] = exclude_session

            rows = db.execute(
                text(
                    f"SELECT e.session_id, e.content, e.created_at, s.title "
                    f"FROM conversation_events e "
                    f"JOIN sessions s ON e.session_id = s.session_id "
                    f"WHERE e.user_id = :user_id AND e.event_type = 'session_summary' "
                    f"{exclude_clause} "
                    f"ORDER BY e.created_at DESC LIMIT :limit"
                ),
                params,
            ).fetchall()

            return [
                {
                    "session_id": r[0],
                    "summary": r[1],
                    "created_at": r[2].isoformat() if r[2] else None,
                    "title": r[3],
                }
                for r in rows
            ]

    def _load_knowledge(
        self, user_id: str, limit: int,
    ) -> list[dict[str, Any]]:
        """Load highest-confidence knowledge entries."""
        with self._db() as db:
            rows = db.execute(
                text(
                    "SELECT entry_id, category, key_name, value, confidence "
                    "FROM sk_knowledge_entries "
                    "WHERE user_id = :user_id AND confidence > 0.3 "
                    "AND superseded_by IS NULL "
                    "ORDER BY confidence DESC, access_count DESC "
                    "LIMIT :limit"
                ),
                {"user_id": user_id, "limit": limit},
            ).fetchall()

            results = [
                {
                    "entry_id": r[0],
                    "category": r[1],
                    "key": r[2],
                    "value": r[3],
                    "confidence": float(r[4]),
                }
                for r in rows
            ]

            # Update access tracking for returned entries
            if results:
                from skills.knowledge.api import update_access_tracking
                update_access_tracking(db, [r["entry_id"] for r in results])

            return results

    def _load_active_notes(
        self, user_id: str, limit: int,
    ) -> list[dict[str, Any]]:
        """Load active scratchpad notes across all sessions."""
        with self._db() as db:
            rows = db.execute(
                text(
                    "SELECT note_id, session_id, note_type, content, updated_at "
                    "FROM agent_scratchpad "
                    "WHERE user_id = :user_id AND status = 'active' "
                    "ORDER BY updated_at DESC LIMIT :limit"
                ),
                {"user_id": user_id, "limit": limit},
            ).fetchall()

            return [
                {
                    "note_id": r[0],
                    "session_id": r[1],
                    "note_type": r[2],
                    "content": r[3],
                    "updated_at": r[4].isoformat() if r[4] else None,
                }
                for r in rows
            ]
