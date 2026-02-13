"""Tool Mocking Layer for safe replay of Agent conversations.

This module intercepts skill executions and provides two modes:
- PRODUCTION: Execute skills and record results
- REPLAY: Return recorded results without re-execution
"""

import hashlib
import json
import logging
from enum import Enum
from typing import Any, Protocol

from sqlalchemy.orm import Session

from api.database import get_db_session
from core.skills.base import SideEffectCategory, Skill

logger = logging.getLogger(__name__)


class MockMode(str, Enum):
    """Execution modes for skill mocking"""

    PRODUCTION = "production"  # Real execution, record results
    REPLAY = "replay"  # Use recorded results, no execution


class SecurityError(Exception):
    """Raised when dangerous operations are blocked in replay mode"""

    pass


class ResultStorage(Protocol):
    """Protocol for pluggable result storage backends.

    Implementations can store results in:
    - Database (default)
    - S3/Object storage (for large results)
    - Redis (for caching)
    - Separate wide table
    """

    def store(self, key: str, result: Any) -> str:
        """Store result and return reference (e.g., S3 URL, DB key)"""
        ...

    def retrieve(self, reference: str) -> Any:
        """Retrieve result by reference"""
        ...


class ToolMockingLayer:
    """Intercepts skill executions for safe replay"""

    def __init__(
        self,
        mode: MockMode,
        session: Session | None = None,
        result_storage: ResultStorage | None = None,
    ):
        self.mode = mode
        self._session = session
        self._owns_session = session is None
        self.result_storage = result_storage

    def _get_session(self) -> Session:
        if self._session is None:
            self._session = next(get_db_session())
        return self._session

    def __del__(self):
        if self._owns_session and self._session:
            self._session.close()

    def execute(
        self,
        skill: Skill,
        params: dict,
        session_id: str,
        parent_event_id: str | None = None,
    ) -> Any:
        """
        Execute skill with mocking logic.

        Args:
            skill: Skill instance to execute
            params: Skill parameters (raw dict)
            session_id: Current session ID
            parent_event_id: Parent event ID for result lookup

        Returns:
            Skill execution result

        Raises:
            SecurityError: If destructive operation in replay mode
        """
        # Safety check: Block destructive operations in replay mode
        if self.mode == MockMode.REPLAY:
            if skill.side_effect_profile.category == SideEffectCategory.DESTRUCTIVE:
                raise SecurityError(
                    f"Blocked destructive skill '{skill.name}' in replay mode. "
                    f"Destructive operations cannot be safely replayed."
                )

            # For WRITE operations, try to use recorded results
            if skill.side_effect_profile.category == SideEffectCategory.WRITE:
                recorded = self._get_recorded_result(
                    skill.name, params, session_id, parent_event_id
                )
                if recorded is not None:
                    logger.info(
                        f"Replay: Using recorded result for {skill.name} (session={session_id})"
                    )
                    return recorded
                else:
                    logger.warning(
                        f"⚠️  DANGEROUS: Executing WRITE skill '{skill.name}' in REPLAY mode "
                        f"due to missing recorded result! This may cause unintended side-effects. "
                        f"(session={session_id}, parent_event={parent_event_id})"
                    )

        # Production mode or READ operations: Execute normally
        # Validate input first
        validated_input = skill.validate_input(params)
        result = skill.execute(validated_input)

        # Record result in production mode
        if self.mode == MockMode.PRODUCTION:
            self._record_result(skill.name, params, result, session_id, parent_event_id)

        return result

    def _get_recorded_result(
        self,
        skill_name: str,
        params: dict,
        session_id: str,
        parent_event_id: str | None,
    ) -> Any | None:
        """
        Query recorded skill result from conversation_events.

        Lookup strategy:
        1. If parent_event_id provided, find exact event (RECOMMENDED for concurrency safety)
        2. Otherwise, find most recent event with matching skill_name and params
           ⚠️  WARNING: Fuzzy lookup is not concurrency-safe in high-load scenarios

        Returns:
            Recorded result or None if not found
        """
        try:
            from api.models import Event as EventModel
            
            # Compute params hash for matching
            params_hash = self._hash_params(params)
            session = self._get_session()

            if parent_event_id:
                # Exact lookup by event ID (concurrency-safe)
                event = session.query(EventModel).filter(
                    EventModel.event_id == parent_event_id,
                    EventModel.skill_name == skill_name
                ).first()
            else:
                # Fuzzy lookup: most recent matching event in session
                logger.warning(
                    f"Using fuzzy lookup for {skill_name} without parent_event_id. "
                    f"This is not concurrency-safe. Consider providing parent_event_id."
                )
                event = session.query(EventModel).filter(
                    EventModel.session_id == session_id,
                    EventModel.skill_name == skill_name,
                    EventModel.event_type == 'tool_result'
                ).order_by(EventModel.created_at.desc()).first()

            if event and event.event_metadata:
                metadata = event.event_metadata

                # Verify params match (optional, for safety)
                recorded_hash = metadata.get("skill_params_hash")
                if recorded_hash and recorded_hash != params_hash:
                    logger.warning(
                        f"Params hash mismatch for {skill_name}: "
                        f"expected {params_hash}, got {recorded_hash}"
                    )
                    return None

                return metadata.get("skill_result")

            return None

        except Exception as e:
            logger.error(f"Failed to get recorded result for {skill_name}: {e}")
            return None

    def _record_result(
        self,
        skill_name: str,
        params: dict,
        result: Any,
        session_id: str,
        parent_event_id: str | None,
    ) -> None:
        """
        Record skill result in conversation_events metadata.

        Note: This assumes the event already exists (created by EventLogger).
        We only UPDATE the metadata with skill_result.

        ⚠️  WARNING: Large results may exceed JSON field limits. Consider:
        - Truncating large results
        - Storing in separate table
        - Using external storage (S3, etc.)
        """
        try:
            params_hash = self._hash_params(params)

            # Serialize result (handle Pydantic models)
            if hasattr(result, "model_dump"):
                result_data = result.model_dump()
            elif hasattr(result, "dict"):
                result_data = result.dict()
            else:
                result_data = result

            metadata = {
                "skill_name": skill_name,
                "skill_params": params,
                "skill_params_hash": params_hash,
                "skill_result": result_data,
            }

            # Check metadata size (MatrixOne JSON field limit is typically 16MB)
            metadata_json = json.dumps(metadata)
            metadata_size_mb = len(metadata_json.encode("utf-8")) / (1024 * 1024)

            if metadata_size_mb > 1.0:  # Warn if > 1MB
                logger.warning(
                    f"Large metadata for {skill_name}: {metadata_size_mb:.2f}MB. "
                    f"Consider truncating or using external storage."
                )

            if metadata_size_mb > 10.0:  # Error if > 10MB
                logger.error(
                    f"Metadata too large for {skill_name}: {metadata_size_mb:.2f}MB. "
                    f"Skipping record to prevent database error."
                )
                # TODO: Use self.result_storage if available
                # if self.result_storage:
                #     ref = self.result_storage.store(f"{session_id}:{skill_name}", result_data)
                #     metadata["skill_result"] = {"stored_at": ref}
                # else:
                #     return
                return

            # Update most recent tool_result event for this skill
            from api.models import Event as EventModel
            
            session = self._get_session()
            event = session.query(EventModel).filter(
                EventModel.session_id == session_id,
                EventModel.skill_name == skill_name,
                EventModel.event_type == 'tool_result'
            ).order_by(EventModel.created_at.desc()).first()
            
            if event:
                event.event_metadata = metadata
                session.commit()

            logger.debug(f"Recorded result for {skill_name} in session {session_id}")

        except Exception as e:
            logger.error(f"Failed to record result for {skill_name}: {e}")

    def _hash_params(self, params: dict) -> str:
        """
        Compute deterministic hash of parameters.

        Used for matching recorded results with current execution.

        ⚠️  LIMITATION: Uses json.dumps(sort_keys=True) for determinism.
        This works for most cases but has edge cases:
        - Nested dicts with non-string keys
        - Custom objects without proper serialization
        - Float precision differences

        For 99% of skill params (flat dicts with primitives), this is sufficient.
        If hash collisions become an issue, consider:
        - Using canonical JSON (e.g., python-canonicaljson)
        - Storing params as separate columns for exact matching
        """
        # Sort keys for deterministic serialization
        params_str = json.dumps(params, sort_keys=True)
        return hashlib.sha256(params_str.encode()).hexdigest()[:16]
