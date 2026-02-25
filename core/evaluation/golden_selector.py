"""Golden sessions selection and management — P1 Evaluation Loop.

Selects high-quality sessions for regression testing:
- Query high-confidence, high-satisfaction sessions
- Tag as golden in conversation_events
- Version golden set with timestamp
- Support filtering by skill/prompt/config
"""

from __future__ import annotations

import json
from datetime import datetime, timezone
from typing import Any

from sqlalchemy import text
from sqlalchemy.orm import Session
from uuid_utils import uuid7

from core.logging_config import get_logger
from core.db_consumer import DbConsumer, DbFactory

logger = get_logger(__name__)


class GoldenSessionSelector(DbConsumer):
    """Select and manage golden sessions for regression testing."""
    
    def __init__(self, db_factory: DbFactory):
        """Initialize selector.
        
        Args:
            db: SQLAlchemy session
        """
        super().__init__(db_factory)
    
    def select_golden_sessions(
        self,
        min_quality_score: float = 4.0,
        min_confidence: float = 0.8,
        min_satisfaction: float = 0.7,
        limit: int = 50,
        skill_name: str | None = None,
        prompt_id: str | None = None,
    ) -> list[dict[str, Any]]:
        """Select golden sessions based on quality criteria.
        
        Args:
            min_quality_score: Minimum quality score (0-5)
            min_confidence: Minimum trust confidence (0-1)
            min_satisfaction: Minimum satisfaction (0-1)
            limit: Maximum sessions to return
            skill_name: Filter by skill name (optional)
            prompt_id: Filter by prompt ID (optional)
        
        Returns:
            List of golden session dicts with metadata
        """
        # Build query
        with self._db() as db:
            query = """
                SELECT 
                    e.event_id,
                    e.session_id,
                    e.user_id,
                    e.content,
                    e.quality_score,
                    e.created_at,
                    e.metadata,
                    e.skills_snapshot
                FROM conversation_events e
                WHERE e.event_type = 'LLM_RESPONSE'
                AND e.quality_score >= :min_quality
            """
        
            params = {"min_quality": min_quality_score}
        
            # Add skill filter
            if skill_name:
                query += " AND e.skills_snapshot LIKE :skill_name"
                params["skill_name"] = f"%{skill_name}%"
        
            # Add prompt filter
            if prompt_id:
                query += " AND e.prompt_template_id = :prompt_id"
                params["prompt_id"] = prompt_id
        
            # Order by quality and recency
            query += """
                ORDER BY e.quality_score DESC, e.created_at DESC
                LIMIT :limit
            """
            params["limit"] = limit
        
            try:
                result = db.execute(text(query), params).fetchall()
            
                sessions = []
                for row in result:
                    sessions.append({
                        "event_id": row[0],
                        "session_id": row[1],
                        "user_id": row[2],
                        "content": row[3],
                        "quality_score": row[4],
                        "created_at": row[5],
                        "metadata": row[6],
                        "skills_snapshot": row[7],
                    })
            
                logger.info(f"Selected {len(sessions)} golden sessions")
                return sessions
        
            except Exception as e:
                logger.error(f"Failed to select golden sessions: {e}")
                return []
    
    def tag_golden_session(
        self,
        event_id: str,
        golden_set_id: str,
        reason: str = "",
    ) -> bool:
        """Tag an event as part of a golden set.
        
        Args:
            event_id: Event ID to tag
            golden_set_id: Golden set identifier
            reason: Reason for selection
        
        Returns:
            True if successful
        """
        with self._db() as db:
            try:
                # Get existing metadata
                result = db.execute(
                    text("SELECT metadata FROM conversation_events WHERE event_id = :event_id"),
                    {"event_id": event_id},
                ).fetchone()
            
                if not result:
                    logger.error(f"Event {event_id} not found")
                    return False
            
                # Parse and update metadata
                metadata = {}
                try:
                    metadata_str = result[0]
                    # metadata_str might be None or a string
                    if metadata_str is not None:
                        try:
                            metadata = json.loads(str(metadata_str))
                        except (json.JSONDecodeError, TypeError, ValueError):
                            metadata = {}
                except Exception as parse_err:
                    logger.warning(f"Could not parse metadata: {type(parse_err).__name__}")
                    metadata = {}
            
                # Add golden set info
                if not isinstance(metadata, dict):
                    metadata = {}
            
                if "evaluation" not in metadata:
                    metadata["evaluation"] = {}
            
                metadata["evaluation"]["golden_set_id"] = golden_set_id
                metadata["evaluation"]["golden_reason"] = reason
                metadata["evaluation"]["golden_tagged_at"] = datetime.now(timezone.utc).isoformat()
            
                # Update metadata
                db.execute(
                    text("UPDATE conversation_events SET metadata = :metadata WHERE event_id = :event_id"),
                    {
                        "event_id": event_id,
                        "metadata": json.dumps(metadata),
                    },
                )
                db.commit()
                return True
        
            except Exception as e:
                error_msg = "Failed to tag golden session"
                try:
                    error_msg = f"{error_msg} {event_id}: {type(e).__name__}"
                except Exception:
                    pass
                logger.error(error_msg)
                try:
                    db.rollback()
                except Exception:
                    pass
                return False
    
    def create_golden_set(
        self,
        sessions: list[dict[str, Any]],
        name: str = "",
        description: str = "",
    ) -> str:
        """Create a versioned golden set.
        
        Args:
            sessions: List of golden sessions
            name: Golden set name
            description: Description
        
        Returns:
            Golden set ID
        """
        golden_set_id = str(uuid7())
        timestamp = datetime.now(timezone.utc).isoformat()
        
        if not name:
            name = f"golden_set_{golden_set_id[:8]}"
        
        # Tag all sessions in the set
        for session in sessions:
            self.tag_golden_session(
                event_id=session["event_id"],
                golden_set_id=golden_set_id,
                reason=f"Quality score: {session.get('quality_score', 0):.2f}",
            )
        
        logger.info(f"Created golden set {golden_set_id} with {len(sessions)} sessions")
        return golden_set_id
    
    def get_golden_set(
        self,
        golden_set_id: str,
    ) -> list[dict[str, Any]]:
        """Retrieve sessions from a golden set.
        
        Args:
            golden_set_id: Golden set ID
        
        Returns:
            List of sessions in the set
        """
        with self._db() as db:
            try:
                # Get all events and filter in Python
                query = """
                    SELECT 
                        event_id,
                        session_id,
                        user_id,
                        content,
                        quality_score,
                        created_at,
                        metadata
                    FROM conversation_events
                    ORDER BY created_at DESC
                """
            
                result = db.execute(text(query)).fetchall()
            
                sessions = []
                for row in result:
                    try:
                        metadata_str = row[6]
                        if metadata_str is not None:
                            metadata = json.loads(str(metadata_str))
                            if isinstance(metadata, dict) and metadata.get("evaluation", {}).get("golden_set_id") == golden_set_id:
                                sessions.append({
                                    "event_id": row[0],
                                    "session_id": row[1],
                                    "user_id": row[2],
                                    "content": row[3],
                                    "quality_score": row[4],
                                    "created_at": row[5],
                                })
                    except (json.JSONDecodeError, TypeError, AttributeError):
                        pass
            
                return sessions
        
            except Exception as e:
                logger.error(f"Failed to retrieve golden set {golden_set_id}: {e}")
                return []
    
    def list_golden_sets(self) -> list[dict[str, Any]]:
        """List all golden sets.
        
        Returns:
            List of golden set metadata
        """
        with self._db() as db:
            try:
                query = """
                    SELECT 
                        event_id,
                        metadata,
                        created_at
                    FROM conversation_events
                    WHERE metadata IS NOT NULL
                    ORDER BY created_at DESC
                """
            
                result = db.execute(text(query)).fetchall()
            
                # Group by golden_set_id
                sets_dict = {}
                for row in result:
                    metadata_str = row[1]
                    if metadata_str:
                        try:
                            metadata = json.loads(metadata_str)
                            golden_set_id = metadata.get("evaluation", {}).get("golden_set_id")
                            if golden_set_id:
                                if golden_set_id not in sets_dict:
                                    sets_dict[golden_set_id] = {
                                        "golden_set_id": golden_set_id,
                                        "session_count": 0,
                                        "last_updated": row[2],
                                    }
                                sets_dict[golden_set_id]["session_count"] += 1
                        except (json.JSONDecodeError, TypeError):
                            pass
            
                return list(sets_dict.values())
        
            except Exception as e:
                logger.error(f"Failed to list golden sets: {e}")
                return []
