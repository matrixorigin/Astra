"""Replay Service - Session replay business logic

Provides session replay functionality for:
1. Verifying system behavior consistency (regression testing)
2. Testing new skill or prompt versions (A/B testing)
3. Regression testing and quality assurance
4. Auditing and debugging historical sessions
"""

from datetime import datetime, timezone
from typing import Dict, Any, Optional
from sqlalchemy.orm import Session
from uuid_utils import uuid7

from api.repositories import SessionRepository, EventRepository
from api.services.exceptions import ResourceNotFoundError, PermissionDeniedError
from core.auth.audit_logger import AuditLogger
from sdk import Database


class ReplayService:
    """Replay business service
    
    Core functionality:
    - replay_session: Replay entire session
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
        self.db = Database()
        self.audit = AuditLogger(self.db)
    
    def replay_session(
        self,
        session_id: str,
        user_id: str,
        sandbox_name: Optional[str] = None,
        mock_mode: bool = True,
        skill_version_override: Optional[Dict[str, str]] = None
    ) -> Dict[str, Any]:
        """Replay a session
        
        Replays all events in a session, optionally in a sandbox environment.
        
        Workflow:
        1. Validate session exists and user has permission
        2. Fetch all events from session (in chronological order)
        3. Replay each event
        4. Generate replay report
        5. Record audit log
        
        Args:
            session_id: Session ID to replay
            user_id: User ID (for permission validation)
            sandbox_name: Sandbox name (optional, for isolated execution)
            mock_mode: Whether to use mock mode (avoid side effects, default True)
                - True: Return original content, no actual execution
                - False: Re-execute skills and LLM calls (requires integration)
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
            
        Example:
            >>> service = ReplayService(db_session)
            >>> result = service.replay_session(
            ...     session_id="sess_123",
            ...     user_id="user_456",
            ...     mock_mode=True
            ... )
            >>> print(result["events_replayed"])
            5
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
        """Replay a single event (internal method)
        
        Executes different replay logic based on event type:
        - user_query: Return original content (user input doesn't need replay)
        - llm_response: Return original response in mock mode, regenerate otherwise
        - skill_invocation: Return original result in mock mode, re-execute otherwise
        - Other types: Return original content
        
        Mock mode vs Actual mode:
        - Mock mode: Fast validation, no side effects, returns original content
        - Actual mode: Full replay, may have side effects, requires SkillRegistry and LLM
        
        Args:
            event: Event object (from EventRepository)
            mock_mode: Whether to use mock mode
            skill_version_override: Skill version override dict
            
        Returns:
            Replay result dict containing:
            - event_id (str): Event ID
            - event_type (str): Event type
            - original_content (str): Original content
            - replayed_content (str): Replay content
            - success (bool): Whether successful
            - mode (str): Mode (mock/actual)
            - note (str): Description
            - created_at (str): Original creation time
            
        Note:
            Current implementation is simplified, returns original content in actual mode.
            Full implementation requires:
            1. SkillRegistry - Load and execute skills
            2. LLM Client - Regenerate responses
            3. Sandbox - Isolated execution environment
        """
        # Build base result
        base_result = {
            "event_id": event.event_id,
            "event_type": event.event_type,
            "original_content": event.content,
            "created_at": event.created_at.isoformat() if hasattr(event, 'created_at') else None
        }
        
        # In mock mode, return original content for all events
        if mock_mode:
            base_result.update({
                "success": True,
                "replayed_content": event.content,
                "mode": "mock",
                "note": "Returned original content in mock mode (no side effects)"
            })
            return base_result
        
        # In actual mode, execute actual replay based on event type
        # Note: Current simplified implementation, returns original content
        # Full implementation requires SkillRegistry and LLM integration
        if event.event_type == "user_query":
            # User query returns as-is (no processing needed)
            base_result.update({
                "success": True,
                "replayed_content": event.content,
                "mode": "actual",
                "note": "User query replayed as-is (no processing needed)"
            })
        elif event.event_type == "llm_response":
            # LLM response: should regenerate in actual mode
            # TODO: Integrate LLM Client to regenerate response
            base_result.update({
                "success": True,
                "replayed_content": event.content,
                "mode": "actual",
                "note": "LLM response - would regenerate with LLM Client in full implementation"
            })
        elif event.event_type == "skill_invocation":
            # Skill invocation: should re-execute in actual mode
            # TODO: Integrate SkillRegistry to re-execute skill
            base_result.update({
                "success": True,
                "replayed_content": event.content,
                "mode": "actual",
                "note": "Skill invocation - would re-execute with SkillRegistry in full implementation"
            })
        else:
            # Other event types
            base_result.update({
                "success": True,
                "replayed_content": event.content,
                "mode": "actual",
                "note": f"Event type '{event.event_type}' replayed"
            })
        
        return base_result
    
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
        
        Comparison dimensions:
        - Event count: Original vs replayed
        - Event content: Per-event comparison
        - Success rate: Percentage of successful replays
        
        Args:
            session_id: Session ID
            user_id: User ID (for permission validation)
            replay_result: Replay result (from replay_session)
            
        Returns:
            Comparison result dict containing:
            - session_id (str): Session ID
            - original_event_count (int): Original event count
            - replay_event_count (int): Replayed event count
            - difference (int): Count difference
            - match (bool): Whether perfectly matched
            - mismatched_events (int): Number of mismatched events
            - details (list): Detailed comparison info (max 10)
                - event_index (int): Event index
                - event_id (str): Event ID
                - event_type (str): Event type
                - original (str): Original content (truncated)
                - replayed (str): Replayed content (truncated)
                - match (bool): Whether matched
            - compared_at (str): Comparison time (ISO format)
            
        Raises:
            PermissionDeniedError: User lacks permission
            
        Example:
            >>> comparison = service.compare_outputs(
            ...     session_id="sess_123",
            ...     user_id="user_456",
            ...     replay_result=replay_result
            ... )
            >>> if comparison["match"]:
            ...     print("Perfect match!")
            >>> else:
            ...     print(f"Found {comparison['mismatched_events']} differences")
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
        for i, (orig, replay) in enumerate(zip(original_events, replay_result.get("events", []))):
            # Check if content matches
            if orig.content != replay.get("replayed_content"):
                details.append({
                    "event_index": i,
                    "event_id": orig.event_id,
                    "event_type": orig.event_type,
                    "original": orig.content[:100],  # Truncate to avoid large response
                    "replayed": replay.get("replayed_content", "")[:100],
                    "match": False
                })
        
        # 5. Build comparison result
        return {
            "session_id": session_id,
            "original_event_count": original_count,
            "replay_event_count": replay_count,
            "difference": replay_count - original_count,
            "match": original_count == replay_count and len(details) == 0,
            "mismatched_events": len(details),
            "details": details[:10],  # Return max 10 differences to avoid large response
            "compared_at": datetime.now(timezone.utc).isoformat()
        }
