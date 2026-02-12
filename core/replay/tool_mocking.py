"""Tool Mocking Layer - Side-effect isolation for safe replay

This module provides the critical safety layer that prevents replay from
triggering real-world side effects (merging PRs, deleting repos, sending emails).

Key concepts:
- Production mode: Execute real skills with real side effects
- Replay mode: Return recorded results from previous execution
- Dry-run mode: Validate inputs but don't execute

Architecture:
1. Recording phase (production): Store skill results and side effects
2. Replay phase (sandbox): Return recorded results instead of re-executing
3. Validation: Ensure all required data is available for replay
"""

import json
from typing import Dict, Any, Optional, Literal
from enum import Enum

from core.logging_config import get_logger
from sdk import Database

logger = get_logger(__name__)


class ExecutionMode(str, Enum):
    """Execution mode for tool invocation
    
    - PRODUCTION: Execute real skills with real side effects
    - REPLAY: Return recorded results (safe, no side effects)
    - DRY_RUN: Validate inputs only (no execution, no side effects)
    """
    PRODUCTION = "production"
    REPLAY = "replay"
    DRY_RUN = "dry_run"


class SideEffectCategory(str, Enum):
    """Side effect category for skills
    
    - READ: Read-only operations (safe to replay)
    - WRITE: Write operations (need mocking in replay)
    - DESTRUCTIVE: Destructive operations (critical to mock)
    """
    READ = "read"
    WRITE = "write"
    DESTRUCTIVE = "destructive"


class ReplayError(Exception):
    """Raised when replay cannot proceed due to missing data"""
    pass


class ToolMockingLayer:
    """Tool mocking layer for safe replay
    
    Provides three-layer isolation:
    1. Data isolation: Separate database (handled by Sandbox)
    2. Execution isolation: Mock skill invocations (this class)
    3. Code isolation: Docker containers (future)
    
    Usage:
        # Production mode
        mocker = ToolMockingLayer(mode=ExecutionMode.PRODUCTION, db=db)
        result = mocker.invoke_skill("github_merge_pr", {"id": 123})
        
        # Replay mode
        mocker = ToolMockingLayer(mode=ExecutionMode.REPLAY, db=db, session_id="sess_123")
        result = mocker.invoke_skill("github_merge_pr", {"id": 123})  # Returns recorded result
    """
    
    def __init__(
        self,
        mode: ExecutionMode,
        db: Database,
        session_id: Optional[str] = None
    ):
        """Initialize tool mocking layer
        
        Args:
            mode: Execution mode (production/replay/dry_run)
            db: Database instance
            session_id: Session ID (required for replay mode)
        """
        self.mode = mode
        self.db = db
        self.session_id = session_id
        self.recorded_results: Dict[str, Any] = {}
        
        # Load recorded results if in replay mode
        if mode == ExecutionMode.REPLAY:
            if not session_id:
                raise ValueError("session_id required for replay mode")
            self._load_recorded_results()
    
    def _load_recorded_results(self) -> None:
        """Load recorded skill results from database
        
        Loads all skill invocation events from the session and builds
        a lookup table: (skill_id, params) -> result
        """
        with self.db.get_cursor() as cursor:
            cursor.execute(
                """
                SELECT skill_name, skill_version, metadata, skill_result
                FROM conversation_events
                WHERE session_id = %s 
                  AND event_type = 'skill_invocation'
                  AND skill_result IS NOT NULL
                ORDER BY created_at
                """,
                (self.session_id,)
            )
            
            events = cursor.fetchall()
            
            for event in events:
                # Parse metadata to get params
                metadata = json.loads(event["metadata"]) if event["metadata"] else {}
                params = metadata.get("skill_params", {})
                
                # Build lookup key
                key = self._make_key(event["skill_name"], params)
                
                # Store result
                self.recorded_results[key] = json.loads(event["skill_result"]) if event["skill_result"] else None
            
            logger.info(f"Loaded {len(self.recorded_results)} recorded skill results for session {self.session_id}")
    
    def _make_key(self, skill_id: str, params: Dict[str, Any]) -> str:
        """Create lookup key for skill invocation
        
        Args:
            skill_id: Skill identifier
            params: Skill parameters
            
        Returns:
            Unique key string
        """
        # Sort params to ensure consistent key
        params_str = json.dumps(params, sort_keys=True)
        return f"{skill_id}:{params_str}"
    
    def invoke_skill(
        self,
        skill_id: str,
        params: Dict[str, Any],
        skill_version: Optional[str] = None
    ) -> Any:
        """Invoke a skill with appropriate isolation
        
        Behavior depends on execution mode:
        - PRODUCTION: Execute real skill
        - REPLAY: Return recorded result
        - DRY_RUN: Validate only
        
        Args:
            skill_id: Skill identifier
            params: Skill parameters
            skill_version: Skill version (optional)
            
        Returns:
            Skill execution result
            
        Raises:
            ReplayError: In replay mode, if no recorded result found
        """
        key = self._make_key(skill_id, params)
        
        if self.mode == ExecutionMode.PRODUCTION:
            # Real execution - delegate to actual skill
            logger.info(f"Executing skill in production mode: {skill_id}")
            return self._execute_real(skill_id, params, skill_version)
        
        elif self.mode == ExecutionMode.REPLAY:
            # Return recorded result
            if key in self.recorded_results:
                logger.info(f"Returning recorded result for: {skill_id}")
                return self.recorded_results[key]
            else:
                logger.warning(f"No recorded result for: {skill_id} with params {params}")
                raise ReplayError(
                    f"No recorded result for skill '{skill_id}' with params {params}. "
                    f"This skill may not have been executed in the original session."
                )
        
        elif self.mode == ExecutionMode.DRY_RUN:
            # Validate only, return mock result
            logger.info(f"Dry-run mode: validating {skill_id}")
            return self._validate_and_mock(skill_id, params)
        
        else:
            raise ValueError(f"Unknown execution mode: {self.mode}")
    
    def _execute_real(
        self,
        skill_id: str,
        params: Dict[str, Any],
        skill_version: Optional[str]
    ) -> Any:
        """Execute real skill (production mode)
        
        TODO: Integrate with SkillRegistry to load and execute skill
        
        Args:
            skill_id: Skill identifier
            params: Skill parameters
            skill_version: Skill version
            
        Returns:
            Skill execution result
        """
        # Placeholder: Real implementation requires SkillRegistry integration
        logger.warning(f"Real execution not yet implemented for {skill_id}")
        return {"status": "not_implemented", "skill_id": skill_id}
    
    def _validate_and_mock(
        self,
        skill_id: str,
        params: Dict[str, Any]
    ) -> Any:
        """Validate skill invocation and return mock result
        
        Args:
            skill_id: Skill identifier
            params: Skill parameters
            
        Returns:
            Mock result
        """
        # Validate params structure
        if not isinstance(params, dict):
            raise ValueError(f"Invalid params type for {skill_id}: expected dict")
        
        # Return mock result
        return {
            "status": "dry_run",
            "skill_id": skill_id,
            "params": params,
            "note": "Validated successfully, no execution"
        }
    
    def record_skill_invocation(
        self,
        event_id: str,
        skill_id: str,
        params: Dict[str, Any],
        result: Any,
        side_effects: Optional[Dict[str, Any]] = None
    ) -> None:
        """Record skill invocation result for future replay
        
        Should be called after every skill execution in production mode.
        
        Args:
            event_id: Event ID
            skill_id: Skill identifier
            params: Skill parameters
            result: Skill execution result
            side_effects: Side effects metadata (API calls, state changes, etc.)
        """
        try:
            self.db.execute(
                """
                UPDATE conversation_events
                SET skill_result = %s, side_effects = %s
                WHERE event_id = %s
                """,
                (
                    json.dumps(result),
                    json.dumps(side_effects or {}),
                    event_id
                )
            )
            logger.info(f"Recorded skill result for event {event_id}")
        except Exception as e:
            logger.error(f"Failed to record skill result: {e}")
            # Don't raise - recording failure shouldn't break production flow
