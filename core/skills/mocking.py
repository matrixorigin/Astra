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

from core.skills.base import SideEffectCategory, Skill

logger = logging.getLogger(__name__)


class MockMode(str, Enum):
    """Execution modes for skill mocking"""

    PRODUCTION = "production"  # Real execution, record results
    REPLAY = "replay"  # Use recorded results, no execution
    DRY_RUN = "dry_run"  # Validate inputs only (no execution, no side effects)


class SecurityError(Exception):
    """Raised when dangerous operations are blocked in replay mode"""

    pass


class ReplayError(Exception):
    """Raised when replay cannot proceed due to missing data"""
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
        session: Session,
        result_storage: ResultStorage | None = None,
        session_id: str | None = None,
    ):
        """Initialize ToolMockingLayer.

        Args:
            mode: Execution mode
            session: SQLAlchemy session (required)
            result_storage: Optional result storage
            session_id: Session ID (required for replay)
        """
        if not isinstance(session, Session):
            raise TypeError("session must be a SQLAlchemy Session")
        
        self.mode = mode
        self.session = session
        self.result_storage = result_storage
        self.session_id = session_id
            
        if self.mode == MockMode.REPLAY and not self.session_id:
            raise ValueError("session_id required for replay mode")

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
                    skill.name, params, session_id, parent_event_id,
                    expected_version=getattr(skill, "version", None),
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

        # If skill.execute is async, run it synchronously
        import asyncio
        import inspect
        if inspect.isawaitable(result):
            try:
                loop = asyncio.get_running_loop()
            except RuntimeError:
                loop = None
            if loop and loop.is_running():
                # Already in async context — create a task
                import concurrent.futures
                with concurrent.futures.ThreadPoolExecutor() as pool:
                    result = pool.submit(asyncio.run, result).result()
            else:
                result = asyncio.run(result)

        # Record result in production mode
        if self.mode == MockMode.PRODUCTION:
            self._record_result(
                skill.name, params, result, session_id, parent_event_id,
                skill_version=getattr(skill, "version", None),
            )

        return result

    def get_mock_result(
        self,
        skill_name: str,
        params: dict,
        session_id: str,
        parent_event_id: str | None = None,
    ) -> Any | None:
        """
        Get recorded result for a skill execution without executing it.
        Useful for ReplayService where Skill object might not be available.
        """
        return self._get_recorded_result(skill_name, params, session_id, parent_event_id)

    def invoke_skill(
        self,
        skill_name: str,
        params: dict,
        skill_version: str | None = None,
        event_id: str | None = None,
    ) -> Any:
        """
        Invoke a skill by name (for ReplayService/external use).

        Behavior depends on execution mode:
        - REPLAY: Return recorded result (using get_mock_result)
        - DRY_RUN: Validate params only (no execution)
        - PRODUCTION: Raises NotImplementedError (requires Skill object, use execute())
        """
        if self.mode == MockMode.REPLAY:
            if not self.session_id:
                raise ValueError("session_id required for replay mode")
            
            result = self.get_mock_result(skill_name, params, self.session_id, parent_event_id=event_id)
            if result is None:
                # Fallback: check if result is stored in side_effects or somewhere else?
                # core/replay/tool_mocking.py raised ReplayError.
                raise ReplayError(
                    f"No recorded result for skill '{skill_name}' with params {params}. "
                    f"This skill may not have been executed in the original session."
                )
            return result

        elif self.mode == MockMode.DRY_RUN:
            # Validate only, return mock result
            if not isinstance(params, dict):
                raise ValueError(f"Invalid params type for {skill_name}: expected dict")
            
            return {
                "status": "dry_run",
                "skill_id": skill_name,
                "params": params,
                "note": "Validated successfully, no execution"
            }

        elif self.mode == MockMode.PRODUCTION:
            raise NotImplementedError(
                f"Real execution not supported via invoke_skill in production mode. "
                f"Use execute() with Skill object instead."
            )
        
        else:
            raise ValueError(f"Unknown execution mode: {self.mode}")

    def _get_recorded_result(
        self,
        skill_name: str,
        params: dict,
        session_id: str,
        parent_event_id: str | None,
        expected_version: str | None = None,
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
            session = self.session

            if parent_event_id:
                # Exact lookup by parent event ID (concurrency-safe)
                # Assumes tool_result event has parent_event_id pointing to invocation event
                event = session.query(EventModel).filter(
                    EventModel.parent_event_id == parent_event_id,
                    EventModel.skill_name == skill_name,
                    EventModel.event_type.in_(['tool_result', 'stream_tool_result'])
                ).first()
            else:
                # Fuzzy lookup: most recent matching event in session
                logger.warning(
                    f"Using fuzzy lookup for {skill_name} without parent_event_id. "
                    f"This is not concurrency-safe. "
                    f"Pass parent_event_id from the tool_call event for deterministic lookups."
                )
                event = session.query(EventModel).filter(
                    EventModel.session_id == session_id,
                    EventModel.skill_name == skill_name,
                    EventModel.event_type.in_(['tool_result', 'stream_tool_result'])
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

                # Warn on version mismatch (result may be stale)
                recorded_version = metadata.get("skill_version")
                if expected_version and recorded_version and recorded_version != expected_version:
                    logger.warning(
                        f"Skill version mismatch for {skill_name}: "
                        f"recorded={recorded_version}, current={expected_version}. "
                        f"Result may be stale."
                    )

                return metadata.get("skill_result")

            return None

        except Exception as e:
            logger.error(f"Failed to get recorded result for {skill_name}: {e}")
            return None

    def record_skill_invocation(
        self,
        event_id: str,
        skill_id: str,
        params: dict,
        result: Any,
        side_effects: dict | None = None,
    ) -> None:
        """Record skill invocation result (for testing/manual recording)."""
        self._record_result(
            skill_name=skill_id,
            params=params,
            result=result,
            session_id=self.session_id or "unknown",  # Fallback if not set
            parent_event_id=None, # Not used in this manual update path
            event_id_override=event_id
        )

    @property
    def recorded_results(self) -> dict:
        """Get all recorded results for the current session (for testing)."""
        if not self.session_id:
            return {}
        
        from api.models import Event as EventModel
        session = self.session
        events = session.query(EventModel).filter(
            EventModel.session_id == self.session_id,
            EventModel.skill_result.isnot(None)
        ).all()
        
        results = {}
        for event in events:
            key = self._make_key(event.skill_name, event.event_metadata.get("skill_params", {}))
            results[key] = event.skill_result
        return results

    def _make_key(self, skill_name: str, params: dict) -> str:
        """Generate a consistent key for skill execution."""
        return f"{skill_name}:{self._hash_params(params)}"

    def _record_result(
        self,
        skill_name: str,
        params: dict,
        result: Any,
        session_id: str,
        parent_event_id: str | None,
        event_id_override: str | None = None,
        skill_version: str | None = None,
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
            if skill_version:
                metadata["skill_version"] = skill_version

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
                return

            # Update event
            from api.models import Event as EventModel
            
            session = self.session
            query = session.query(EventModel)
            
            if event_id_override:
                query = query.filter(EventModel.event_id == event_id_override)
            elif parent_event_id:
                # Concurrency-safe: locate by parent event chain
                query = query.filter(
                    EventModel.parent_event_id == parent_event_id,
                    EventModel.skill_name == skill_name,
                    EventModel.event_type.in_(['tool_result', 'stream_tool_result'])
                )
            else:
                # Fallback: most recent matching event in session (not concurrency-safe)
                logger.warning(
                    f"Recording result for {skill_name} without event_id_override or "
                    f"parent_event_id — not concurrency-safe. "
                    f"Pass parent_event_id from the tool_call event for safe lookups."
                )
                query = query.filter(
                    EventModel.session_id == session_id,
                    EventModel.skill_name == skill_name,
                    EventModel.event_type.in_(['tool_result', 'stream_tool_result'])
                ).order_by(EventModel.created_at.desc())
            
            event = query.first()
            
            if event:
                # Merge with existing metadata if present
                existing_metadata = event.event_metadata or {}
                existing_metadata.update(metadata)
                event.event_metadata = existing_metadata
                
                # Also update columns for easier access
                event.skill_result = result_data
                
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
