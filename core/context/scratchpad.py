"""Agent scratchpad for working memory.

Structured notes that persist across sessions for long-horizon tasks.
Follows Claude Code's CLAUDE.md pattern - agent reads and writes notes as a tool.
"""

import uuid
from datetime import datetime
from typing import Any, Literal
from sqlalchemy.orm import Session

from core.utils.id_generator import generate_note_id
from uuid_utils import uuid7
from core.logging_config import get_logger
from core.db_consumer import DbConsumer, DbFactory

logger = get_logger(__name__)

# Valid note types
NoteType = Literal["plan", "hypothesis", "finding", "todo", "decision"]


class AgentScratchpad(DbConsumer):
    """Working memory for long-horizon tasks.

    Maintains structured notes that survive context compaction and session boundaries.
    Agent can create, read, update, and close notes as needed.

    Note types:
    - plan: High-level task decomposition
    - hypothesis: Current working theories
    - finding: Discovered facts or patterns
    - todo: Pending actions
    - decision: Architectural decisions and rationale

    Example:
        >>> scratchpad = AgentScratchpad(db)
        >>> note_id = scratchpad.create_note(
        ...     session_id="sess_123",
        ...     user_id="alice",
        ...     note_type="plan",
        ...     content="1. Analyze auth flow\n2. Identify bottleneck\n3. Propose fix"
        ... )
        >>> notes = scratchpad.get_active_notes("sess_123")
        >>> scratchpad.close_note(note_id, status="completed")
    """

    def __init__(self, db_factory: DbFactory):
        super().__init__(db_factory)

    def create_note(
        self,
        session_id: str,
        user_id: str,
        note_type: str,
        content: str,
        agent_id: str | None = None,
        related_event_ids: list[str] | None = None,
    ) -> str:
        """Create a new scratchpad note.

        Args:
            session_id: Current session
            user_id: User who owns the note
            note_type: Type of note (plan, hypothesis, finding, todo, decision)
            content: Note content
            agent_id: Agent that created the note
            related_event_ids: Events that produced this note

        Returns:
            Note ID

        Raises:
            ValueError: If note_type is invalid
        """
        with self._db() as db:
            from api.models import AgentScratchpad as ScratchpadModel

            # Validate note type
            valid_types = {"plan", "hypothesis", "finding", "todo", "decision"}
            if note_type not in valid_types:
                raise ValueError(f"Invalid note_type: {note_type}. Must be one of {valid_types}")

            note_id = f"note_{generate_note_id()}"

            note = ScratchpadModel(
                note_id=note_id,
                session_id=session_id,
                user_id=user_id,
                agent_id=agent_id,
                note_type=note_type,
                content=content,
                status="active",
                related_event_ids=related_event_ids or [],
            )

            db.add(note)
            db.commit()

            logger.info(f"Created {note_type} note {note_id} for session {session_id}")
            return note_id

    def get_active_notes(
        self,
        session_id: str,
        note_type: str | None = None,
    ) -> list[dict[str, Any]]:
        """Get active notes for a session.

        Args:
            session_id: Session ID
            note_type: Optional filter by note type

        Returns:
            List of active notes
        """
        with self._db() as db:
            from api.models import AgentScratchpad as ScratchpadModel

            query = db.query(ScratchpadModel).filter(
                ScratchpadModel.session_id == session_id, ScratchpadModel.status == "active"
            )

            if note_type:
                query = query.filter(ScratchpadModel.note_type == note_type)

            notes = query.order_by(ScratchpadModel.created_at).all()

            return [
                {
                    "note_id": n.note_id,
                    "note_type": n.note_type,
                    "content": n.content,
                    "created_at": n.created_at.isoformat() if n.created_at else None,
                    "related_event_ids": n.related_event_ids,
                }
                for n in notes
            ]

    def get_cross_session_notes(
        self,
        user_id: str,
        note_type: str | None = None,
        limit: int = 10,
    ) -> list[dict[str, Any]]:
        """Get active notes across all user sessions.

        Enables cross-session continuity - agent can see notes from previous sessions.

        Args:
            user_id: User ID
            note_type: Optional filter by note type
            limit: Max notes to return (default 10, ordered by most recent update)

        Returns:
            List of active notes across sessions, most recently updated first
        """
        with self._db() as db:
            from api.models import AgentScratchpad as ScratchpadModel

            query = db.query(ScratchpadModel).filter(
                ScratchpadModel.user_id == user_id, ScratchpadModel.status == "active"
            )

            if note_type:
                query = query.filter(ScratchpadModel.note_type == note_type)

            notes = query.order_by(ScratchpadModel.updated_at.desc()).limit(limit).all()

            return [
                {
                    "note_id": n.note_id,
                    "session_id": n.session_id,
                    "note_type": n.note_type,
                    "content": n.content,
                    "created_at": n.created_at.isoformat() if n.created_at else None,
                    "updated_at": n.updated_at.isoformat() if n.updated_at else None,
                }
                for n in notes
            ]

    def update_note(
        self,
        note_id: str,
        content: str,
        append: bool = False,
    ) -> bool:
        """Update note content.

        Args:
            note_id: Note ID
            content: New content
            append: If True, append to existing content

        Returns:
            True if updated
        """
        with self._db() as db:
            from api.models import AgentScratchpad as ScratchpadModel

            note = db.query(ScratchpadModel).filter(ScratchpadModel.note_id == note_id).first()

            if not note:
                logger.warning(f"Note {note_id} not found")
                return False

            if append:
                note.content = f"{note.content}\n\n{content}"
            else:
                note.content = content

            note.updated_at = datetime.now()
            db.commit()

            logger.debug(f"Updated note {note_id}")
            return True

    def close_note(
        self,
        note_id: str,
        status: str = "completed",
    ) -> bool:
        """Close a note.

        Args:
            note_id: Note ID
            status: New status (completed, superseded)

        Returns:
            True if closed
        """
        with self._db() as db:
            from api.models import AgentScratchpad as ScratchpadModel

            note = db.query(ScratchpadModel).filter(ScratchpadModel.note_id == note_id).first()

            if not note:
                logger.warning(f"Note {note_id} not found")
                return False

            note.status = status
            note.updated_at = datetime.now()
            db.commit()

            logger.info(f"Closed note {note_id} with status {status}")
            return True

    def link_notes(
        self,
        note_id: str,
        related_note_ids: list[str],
    ) -> bool:
        """Link related notes.

        Args:
            note_id: Note ID
            related_note_ids: IDs of related notes

        Returns:
            True if linked
        """
        with self._db() as db:
            from api.models import AgentScratchpad as ScratchpadModel

            note = db.query(ScratchpadModel).filter(ScratchpadModel.note_id == note_id).first()

            if not note:
                logger.warning(f"Note {note_id} not found")
                return False

            note.related_note_ids = related_note_ids
            note.updated_at = datetime.now()
            db.commit()

            logger.debug(f"Linked note {note_id} to {len(related_note_ids)} notes")
            return True
