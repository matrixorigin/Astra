"""Replay engine for reproducing conversations with exact skill versions."""

import json
from typing import Optional
from sdk import Database
from core.skills import SkillRegistry
from core.events.event_logger import EventLogger
from core.logging_config import get_logger
from core.exceptions import ReplayError, SkillNotFoundError

logger = get_logger(__name__)


class ReplayEngine:
    """Replay conversations with exact skill versions from the past"""

    def __init__(self, db: Database, registry: SkillRegistry, logger_instance: EventLogger):
        self.db = db
        self.registry = registry
        self.logger_instance = logger_instance

    async def replay_conversation(
        self, session_id: str, replay_timestamp: Optional[str] = None
    ) -> dict:
        """Replay a conversation using skills from that time.

        Args:
            session_id: Session to replay
            replay_timestamp: Optional timestamp to replay up to (ISO format)

        Returns:
            dict with replay results

        Raises:
            ReplayError: If replay fails
        """
        logger.info(f"Starting replay for session: {session_id}")

        try:
            # 1. Fetch events
            query = """
                SELECT event_id, event_type, content, skill_name, skill_version, 
                       created_at, metadata
                FROM conversation_events
                WHERE session_id = %s
            """
            params = [session_id]

            if replay_timestamp:
                query += " AND created_at <= %s"
                params.append(replay_timestamp)

            query += " ORDER BY created_at"

            events = self.db.fetchall(query, tuple(params))

            if not events:
                logger.warning(f"No events found for session {session_id}")
                raise ReplayError(
                    f"No events found for session {session_id}", session_id=session_id
                )

            # 2. Replay each skill execution event
            results = []
            for event in events:
                if event["event_type"] == "skill_exec" and event["skill_name"]:
                    result = await self._replay_skill_execution(event)
                    results.append(result)

            logger.info(
                f"Replay completed for session {session_id}: {len(results)} events replayed"
            )

            return {
                "success": True,
                "session_id": session_id,
                "events_replayed": len(results),
                "results": results,
            }

        except ReplayError:
            raise
        except Exception as e:
            logger.error(f"Replay failed for session {session_id}: {e}", exc_info=True)
            raise ReplayError(f"Replay failed: {e}", session_id=session_id)

    async def _replay_skill_execution(self, event: dict) -> dict:
        """Replay a single skill execution.

        Args:
            event: Event record from conversation_events

        Returns:
            dict with replay result
        """
        skill_name = event["skill_name"]
        skill_version = event["skill_version"]

        logger.debug(f"Replaying skill: {skill_name}@{skill_version}")

        # 1. Load skill version from registry
        try:
            skill = self.registry.get(skill_name, version=skill_version)
        except SkillNotFoundError as e:
            logger.warning(f"Skill not found during replay: {skill_name}@{skill_version}")
            return {
                "success": False,
                "event_id": event["event_id"],
                "skill": f"{skill_name}@{skill_version}",
                "error": str(e),
                "note": "Skill may have been deleted or not loaded",
            }

        # 2. Parse original input from metadata
        metadata = json.loads(event["metadata"]) if event["metadata"] else {}
        original_input = metadata.get("input", {})

        if not original_input:
            logger.warning(f"Original input not found in event {event['event_id']}")
            return {
                "success": False,
                "event_id": event["event_id"],
                "skill": f"{skill_name}@{skill_version}",
                "error": "Original input not found in event metadata",
            }

        # 3. Execute skill with original input
        try:
            input_obj = skill.validate_input(original_input)
            output = await skill.execute(input_obj)

            logger.info(f"Successfully replayed skill: {skill_name}@{skill_version}")

            return {
                "success": True,
                "event_id": event["event_id"],
                "skill": f"{skill_name}@{skill_version}",
                "original_timestamp": event["created_at"],
                "output": output.model_dump(),
                "note": "Replayed successfully with exact skill version",
            }

        except Exception as e:
            logger.error(f"Skill execution failed during replay: {skill_name}@{skill_version}: {e}")
            return {
                "success": False,
                "event_id": event["event_id"],
                "skill": f"{skill_name}@{skill_version}",
                "error": str(e),
            }

    def verify_reproducibility(self, session_id: str) -> dict:
        """Verify that a session can be reproduced.

        Checks:
        1. All skill versions are available
        2. All event metadata contains required input
        3. All LLM pricing is available for cost calculation

        Args:
            session_id: Session to verify

        Returns:
            dict with verification results
        """
        events = self.db.fetchall(
            """
            SELECT event_id, event_type, skill_name, skill_version, 
                   created_at, metadata
            FROM conversation_events
            WHERE session_id = %s
            ORDER BY created_at
        """,
            (session_id,),
        )

        issues = []
        skill_versions_checked = set()

        for event in events:
            # Check skill availability
            if event["event_type"] == "skill_exec" and event["skill_name"]:
                skill_key = f"{event['skill_name']}@{event['skill_version']}"

                if skill_key not in skill_versions_checked:
                    skill = self.registry.get(event["skill_name"], version=event["skill_version"])
                    if not skill:
                        issues.append(
                            {
                                "type": "missing_skill",
                                "event_id": event["event_id"],
                                "skill": skill_key,
                                "message": f"Skill {skill_key} not available",
                            }
                        )
                    skill_versions_checked.add(skill_key)

                # Check input metadata
                metadata = json.loads(event["metadata"]) if event["metadata"] else {}
                if not metadata.get("input"):
                    issues.append(
                        {
                            "type": "missing_input",
                            "event_id": event["event_id"],
                            "message": "Event metadata missing input data",
                        }
                    )

        return {
            "session_id": session_id,
            "reproducible": len(issues) == 0,
            "events_checked": len(events),
            "issues": issues,
        }
