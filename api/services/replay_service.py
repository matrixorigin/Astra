"""Replay Service - Session replay business logic with side-effect isolation

Provides session replay functionality for:
1. Verifying system behavior consistency (regression testing)
2. Testing new skill or prompt versions (A/B testing)
3. Regression testing and quality assurance
4. Auditing and debugging historical sessions

Key feature: ToolMockingLayer integration for safe replay without side effects
"""

import json
from datetime import datetime, timezone
from typing import Dict, Any, Optional
from sqlalchemy.orm import Session
from uuid_utils import uuid7

from api.repositories import SessionRepository, EventRepository
from api.services.exceptions import ResourceNotFoundError, PermissionDeniedError
from core.auth.audit_logger import AuditLogger
from core.skills.mocking import ToolMockingLayer, MockMode


class ReplayService:
    """Replay business service with side-effect isolation
    
    Core functionality:
    - replay_session: Replay entire session with ToolMockingLayer
    - compare_outputs: Compare original and replayed outputs
    - _replay_event: Replay single event (internal method)
    """
    
    def __init__(self, db_session: Session):
        """Initialize Replay service
        
        Args:
            db_session: SQLAlchemy database session
        """
        self.db_session = db_session
        self.session_repo = SessionRepository(db_session)
        self.event_repo = EventRepository(db_session)
        self.audit = AuditLogger(db_session)
    
    def replay_session(
        self,
        session_id: str,
        user_id: str,
        sandbox_name: Optional[str] = None,
        mock_mode: bool = True,
        skill_version_override: Optional[Dict[str, str]] = None
    ) -> Dict[str, Any]:
        """Replay a session with side-effect isolation
        
        Uses ToolMockingLayer to prevent real-world side effects during replay.
        
        Workflow:
        1. Validate session exists and user has permission
        2. Fetch all events from session (in chronological order)
        3. Replay each event through ToolMockingLayer
        4. Generate replay report
        5. Record audit log
        
        Args:
            session_id: Session ID to replay
            user_id: User ID (for permission validation)
            sandbox_name: Sandbox name (optional, for isolated execution)
            mock_mode: Whether to use mock mode (avoid side effects, default True)
                - True: Return recorded results, no actual execution
                - False: Re-execute skills (requires integration)
            skill_version_override: Skill version override (optional, for testing new versions)
                Format: {"skill_name": "version"}
                Example: {"code_review": "2.0.0"}
            
        Returns:
            Replay result dict containing:
            - replay_id (str): Replay ID
            - session_id (str): Session ID
            - status (str): Status (completed/failed)
            - events_replayed (int): Number of replayed events
            - result (dict): Detailed results
                - events (list): List of replayed events
                - total (int): Total event count
                - successful (int): Successful count
                - failed (int): Failed count
            - sandbox_name (str|None): Sandbox name used
            - mock_mode (bool): Whether mock mode was used
            - created_at (str): Creation time (ISO format)
            
        Raises:
            ResourceNotFoundError: Session not found
            PermissionDeniedError: User lacks permission
        """
        # 1. Validate session exists and user has permission
        session = self.session_repo.get_by_id(session_id)
        if not session:
            raise ResourceNotFoundError(f"Session {session_id} not found")
        
        if session.user_id != user_id:
            raise PermissionDeniedError(f"Permission denied for Session {session_id}")
        
        try:
            # 2. Fetch all events from session (in chronological order)
            events, total = self.event_repo.get_by_session(session_id)
            
            # 3. Replay each event
            replayed_events = []
            for event in events:
                replayed_event = self._replay_event(
                    event=event,
                    mock_mode=mock_mode,
                    skill_version_override=skill_version_override
                )
                replayed_events.append(replayed_event)
            
            # 4. Generate replay ID
            replay_id = str(uuid7())
            
            # 5. Build replay result
            replay_result = {
                "events": replayed_events,
                "total": total,
                "successful": sum(1 for e in replayed_events if e.get("success", False)),
                "failed": sum(1 for e in replayed_events if not e.get("success", False))
            }
            
            # 6. Record audit log
            self.audit.log(
                user_id=user_id,
                action="session_replay",
                resource_type="session",
                resource_id=session_id,
                details={
                    "replay_id": replay_id,
                    "sandbox_name": sandbox_name,
                    "mock_mode": mock_mode,
                    "events_count": total,
                    "successful": replay_result["successful"],
                    "failed": replay_result["failed"]
                },
                status="success"
            )
            
            return {
                "replay_id": replay_id,
                "session_id": session_id,
                "status": "completed",
                "events_replayed": total,
                "sandbox_name": sandbox_name,
                "mock_mode": mock_mode,
                "result": replay_result,
                "created_at": datetime.now(timezone.utc).isoformat()
            }
            
        except Exception as e:
            # Record audit failure
            self.audit.log(
                user_id=user_id,
                action="session_replay",
                resource_type="session",
                resource_id=session_id,
                details={"error": str(e)},
                status="failed"
            )
            raise
    
    def _replay_event(
        self,
        event: Any,
        mock_mode: bool,
        skill_version_override: Optional[Dict[str, str]]
    ) -> Dict[str, Any]:
        """Replay a single event with side-effect isolation
        
        Uses ToolMockingLayer for safe replay:
        - Mock mode: Return recorded results (no side effects)
        - Actual mode: Re-execute skills (requires integration)
        
        Args:
            event: Event object (from EventRepository)
            mock_mode: Whether to use mock mode
            skill_version_override: Skill version override dict
            
        Returns:
            Replay result dict containing:
            - event_id (str): Event ID
            - event_type (str): Event type
            - success (bool): Whether replay succeeded
            - content (str): Replayed content
            - error (str|None): Error message if failed
        """
        # Initialize ToolMockingLayer
        execution_mode = MockMode.REPLAY if mock_mode else MockMode.PRODUCTION
        mocker = ToolMockingLayer(
            mode=execution_mode,
            session=self.db_session,
            session_id=event.session_id if mock_mode else None
        )
        
        try:
            if event.event_type == "skill_invocation":
                # Replay skill invocation
                skill_name = event.skill_name
                skill_version = skill_version_override.get(skill_name) if skill_version_override else event.skill_version
                
                # Parse skill params from metadata
                metadata = event.metadata if isinstance(event.metadata, dict) else (json.loads(event.metadata) if event.metadata else {})
                skill_params = metadata.get("skill_params", {})
                
                # Invoke skill through mocking layer
                result = mocker.invoke_skill(
                    skill_name=skill_name,
                    params=skill_params,
                    skill_version=skill_version,
                    event_id=event.event_id  # Pass event_id for exact result lookup
                )
                
                return {
                    "event_id": event.event_id,
                    "event_type": event.event_type,
                    "success": True,
                    "content": json.dumps(result),
                    "error": None
                }
            
            else:
                # For other event types, return original content
                return {
                    "event_id": event.event_id,
                    "event_type": event.event_type,
                    "success": True,
                    "content": event.content,
                    "error": None
                }
        
        except Exception as e:
            return {
                "event_id": event.event_id,
                "event_type": event.event_type,
                "success": False,
                "content": None,
                "error": str(e)
            }
    
    def compare_outputs(
        self,
        session_id: str,
        user_id: str,
        replay_result: Dict[str, Any]
    ) -> Dict[str, Any]:
        """Compare original outputs with replayed outputs
        
        Compares original session with replayed results for:
        1. Verifying system behavior consistency (consistency check)
        2. Detecting regression issues (regression detection)
        3. Assessing new version impact (impact assessment)
        4. Quality assurance (QA)
        
        Args:
            session_id: Session ID
            user_id: User ID (for permission validation)
            replay_result: Replay result (from replay_session)
            
        Returns:
            Comparison result dict
            
        Raises:
            PermissionDeniedError: User lacks permission
        """
        # 1. Validate permission
        session = self.session_repo.get_by_id(session_id)
        if not session or session.user_id != user_id:
            raise PermissionDeniedError(f"Permission denied for Session {session_id}")
        
        # 2. Fetch original events
        original_events, _ = self.event_repo.get_by_session(session_id)
        
        # 3. Simple comparison: Calculate event count difference
        original_count = len(original_events)
        replay_count = len(replay_result.get("events", []))
        
        # 4. Detailed comparison: Per-event content comparison
        details = []
        mismatched = 0
        for i, (orig, replay) in enumerate(zip(original_events, replay_result.get("events", []))):
            # Check if content matches
            if orig.content != replay.get("content"):
                mismatched += 1
                details.append({
                    "event_index": i,
                    "event_id": orig.event_id,
                    "event_type": orig.event_type,
                    "original": orig.content[:100] if orig.content else "",
                    "replayed": replay.get("content", "")[:100],
                    "match": False
                })
        
        return {
            "session_id": session_id,
            "original_event_count": original_count,
            "replay_event_count": replay_count,
            "difference": abs(original_count - replay_count),
            "match": original_count == replay_count and mismatched == 0,
            "mismatched_events": mismatched,
            "details": details[:10],  # Limit to 10 for readability
            "compared_at": datetime.now(timezone.utc).isoformat()
        }
