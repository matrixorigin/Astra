"""Task board for agent teams coordination.

Design ref: agents-and-orchestration.md §5 "Agent Teams"

Task board is implemented via agent_events with special event_type values:
- team_task: Lead creates a task
- team_task_claimed: Member claims a task (creates lock)
- team_task_done: Member completes a task
- agent_message: Peer-to-peer messaging between agents

All coordination is auditable via causal chains and event replay.
Distributed-safe: event creation is atomic, claims use optimistic locking via parent_event_id.
"""

from __future__ import annotations

import json
import logging
from dataclasses import dataclass
from datetime import datetime, timezone
from typing import Any

from sqlalchemy import text

from api.models.agent import Event
from core.db_consumer import DbConsumer, DbFactory

logger = logging.getLogger(__name__)


@dataclass
class Task:
    """A team task."""

    task_id: str
    team_id: str
    title: str
    description: str
    status: str  # "open" | "claimed" | "done" | "failed"
    assigned_to: str | None
    created_by: str
    created_at: datetime
    claimed_at: datetime | None = None
    completed_at: datetime | None = None
    result: str | None = None


class TaskBoard(DbConsumer):
    """Coordinate tasks for agent teams.

    Distributed-safe: all operations are event-based with atomic inserts.
    No explicit locking — claims are recorded as child events.
    """

    def __init__(self, db_factory: DbFactory, event_logger=None) -> None:
        super().__init__(db_factory)
        self.event_logger = event_logger

    def create_task(
        self,
        team_id: str,
        title: str,
        description: str,
        created_by: str,
        session_id: str,
        parent_event_id: str | None = None,
    ) -> str:
        """Create a new task on the board.

        Args:
            team_id: Team identifier
            title: Task title
            description: Task description
            created_by: Agent ID creating the task
            session_id: Session ID
            parent_event_id: Optional parent event for causal chain

        Returns:
            task_id (event_id)
        """
        with self._db() as db:
            if self.event_logger:
                event = self.event_logger.create_event(
                    user_id="system",
                    session_id=session_id,
                    event_type="team_task",
                    content=title,
                    metadata={
                        "team_id": team_id,
                        "description": description,
                        "status": "open",
                        "assigned_to": None,
                        "created_by": created_by,
                    },
                    parent_event_id=parent_event_id,
                )
                return event.event_id
            else:
                # Fallback: direct DB insert
                from core.utils.id_generator import generate_id

                task_id = generate_id()
                # causal_chain_id = task_id: task is the root of its own causal chain
                db.execute(
                    text(
                        "INSERT INTO agent_events "
                        "(event_id, session_id, user_id, agent_id, agent_version, "
                        "event_type, content, metadata, causal_chain_id, parent_event_id, created_at) "
                        "VALUES (:id, :sid, 'system', 'system', '1.0.0', "
                        "'team_task', :title, :meta, :id, :parent, NOW())"
                    ),
                    {
                        "id": task_id,
                        "sid": session_id,
                        "title": title,
                        "meta": json.dumps({
                            "team_id": team_id,
                            "description": description,
                            "status": "open",
                            "assigned_to": None,
                            "created_by": created_by,
                        }),
                        "parent": parent_event_id,
                    },
                )
                db.commit()
                return task_id

    def claim_task(
        self,
        task_id: str,
        agent_id: str,
        session_id: str,
    ) -> bool:
        """Claim a task (optimistic lock via event creation).

        Args:
            task_id: Task event ID
            agent_id: Agent claiming the task
            session_id: Session ID

        Returns:
            True if claimed successfully, False if already claimed
        """
        # Check if already claimed
        with self._db() as db:
            existing = db.execute(
                text(
                    "SELECT COUNT(*) FROM agent_events "
                    "WHERE parent_event_id = :task_id AND event_type = 'team_task_claimed'"
                ),
                {"task_id": task_id},
            ).scalar()

            if existing > 0:
                logger.warning(f"Task {task_id} already claimed")
                return False

            # Create claim event (atomic insert)
            if self.event_logger:
                self.event_logger.create_event(
                    user_id="system",
                    session_id=session_id,
                    event_type="team_task_claimed",
                    content=f"Claimed by {agent_id}",
                    metadata={"claimed_by": agent_id},
                    parent_event_id=task_id,
                )
            else:
                from core.utils.id_generator import generate_id

                eid = generate_id()
                # causal_chain_id = eid: claim is an independent action, linked to task via parent_event_id
                db.execute(
                    text(
                        "INSERT INTO agent_events "
                        "(event_id, session_id, user_id, agent_id, agent_version, "
                        "event_type, content, metadata, causal_chain_id, parent_event_id, created_at) "
                        "VALUES (:id, :sid, 'system', 'system', '1.0.0', "
                        "'team_task_claimed', :content, :meta, :id, :parent, NOW())"
                    ),
                    {
                        "id": eid,
                        "sid": session_id,
                        "content": f"Claimed by {agent_id}",
                        "meta": json.dumps({"claimed_by": agent_id}),
                        "parent": task_id,
                    },
                )
                db.commit()

            logger.info(f"Task {task_id} claimed by {agent_id}")
            return True

    def complete_task(
        self,
        task_id: str,
        agent_id: str,
        result: str,
        session_id: str,
    ) -> None:
        """Mark a task as done.

        Args:
            task_id: Task event ID
            agent_id: Agent completing the task
            result: Result summary
            session_id: Session ID
        """
        with self._db() as db:
            if self.event_logger:
                self.event_logger.create_event(
                    user_id="system",
                    session_id=session_id,
                    event_type="team_task_done",
                    content=result,
                    metadata={"completed_by": agent_id},
                    parent_event_id=task_id,
                )
            else:
                from core.utils.id_generator import generate_id

                eid = generate_id()
                # causal_chain_id = eid: completion is an independent action, linked to task via parent_event_id
                db.execute(
                    text(
                        "INSERT INTO agent_events "
                        "(event_id, session_id, user_id, agent_id, agent_version, "
                        "event_type, content, metadata, causal_chain_id, parent_event_id, created_at) "
                        "VALUES (:id, :sid, 'system', 'system', '1.0.0', "
                        "'team_task_done', :content, :meta, :id, :parent, NOW())"
                    ),
                    {
                        "id": eid,
                        "sid": session_id,
                        "content": result,
                        "meta": json.dumps({"completed_by": agent_id}),
                        "parent": task_id,
                    },
                )
                db.commit()

            logger.info(f"Task {task_id} completed by {agent_id}")

    def get_open_tasks(self, team_id: str, session_id: str) -> list[Task]:
        """Get all open tasks for a team.

        Uses (session_id, created_at) composite index, then Python-side
        metadata filter for team_id/status (avoids JSON_EXTRACT in WHERE).
        """
        with self._db() as db:
            rows = (
                db.query(Event.event_id, Event.content, Event.event_metadata, Event.created_at)
                .filter(Event.session_id == session_id, Event.event_type == "team_task")
                .order_by(Event.created_at.asc())
                .all()
            )

            tasks = []
            for eid, title, meta, created_at in rows:
                meta = meta or {}
                if meta.get("team_id") != team_id or meta.get("status") != "open":
                    continue
                tasks.append(
                    Task(
                        task_id=eid,
                        team_id=team_id,
                        title=title,
                        description=meta.get("description", ""),
                        status="open",
                        assigned_to=None,
                        created_by=meta.get("created_by", ""),
                        created_at=created_at,
                    )
                )
            return tasks

    def send_message(
        self,
        to_agent: str,
        content: str,
        from_agent: str,
        session_id: str,
        causal_chain_id: str | None = None,
    ) -> str:
        """Send a peer-to-peer message between agents.

        Args:
            to_agent: Recipient agent ID
            content: Message content
            from_agent: Sender agent ID
            session_id: Session ID
            causal_chain_id: Optional causal chain

        Returns:
            message_id (event_id)
        """
        with self._db() as db:
            if self.event_logger:
                event = self.event_logger.create_event(
                    user_id="system",
                    session_id=session_id,
                    event_type="agent_message",
                    content=content,
                    metadata={"to_agent": to_agent, "from_agent": from_agent},
                    causal_chain_id=causal_chain_id,
                )
                return event.event_id
            else:
                from core.utils.id_generator import generate_id

                msg_id = generate_id()
                db.execute(
                    text(
                        "INSERT INTO agent_events "
                        "(event_id, session_id, user_id, agent_id, agent_version, "
                        "event_type, content, metadata, causal_chain_id, created_at) "
                        "VALUES (:id, :sid, 'system', 'system', '1.0.0', "
                        "'agent_message', :content, :meta, :chain, NOW())"
                    ),
                    {
                        "id": msg_id,
                        "sid": session_id,
                        "content": content,
                        "meta": json.dumps({"to_agent": to_agent, "from_agent": from_agent}),
                        "chain": causal_chain_id,
                    },
                )
                db.commit()
                return msg_id

    def get_messages_for_agent(
        self, agent_id: str, session_id: str, limit: int = 10,
    ) -> list[dict[str, Any]]:
        """Get recent messages for an agent.

        Uses (session_id, created_at) composite index, then Python-side
        metadata filter for to_agent (avoids JSON_EXTRACT in WHERE).
        """
        with self._db() as db:
            rows = (
                db.query(Event.event_id, Event.content, Event.event_metadata, Event.created_at)
                .filter(Event.session_id == session_id, Event.event_type == "agent_message")
                .order_by(Event.created_at.desc())
                .limit(limit * 5)
                .all()
            )

            messages = []
            for msg_id, content, meta, created_at in rows:
                meta = meta or {}
                if meta.get("to_agent") != agent_id:
                    continue
                messages.append({
                    "message_id": msg_id,
                    "from_agent": meta.get("from_agent", ""),
                    "content": content,
                    "created_at": created_at.isoformat() if created_at else None,
                })
                if len(messages) >= limit:
                    break
            return messages
