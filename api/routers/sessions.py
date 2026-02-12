"""FastAPI sessions router."""

from fastapi import APIRouter, Depends, HTTPException, status

from api.dependencies import get_current_user, get_db
from core.events.session_manager import SessionManager
from sdk import Database
from schemas.session import SessionCreateRequest, SessionListResponse, SessionResponse

router = APIRouter()


def get_session_manager(db: Database = Depends(get_db)) -> SessionManager:
    """Get session manager dependency."""
    return SessionManager(db)


@router.post("", response_model=SessionResponse, status_code=status.HTTP_201_CREATED)
def create_session(
    request: SessionCreateRequest,
    current_user: dict = Depends(get_current_user),
    session_manager: SessionManager = Depends(get_session_manager),
):
    """Create a new session."""
    session = session_manager.create_session(
        user_id=current_user["user_id"],
        metadata=request.metadata,
    )
    
    return SessionResponse(
        session_id=session.session_id,
        user_id=session.user_id,
        status=session.status.value,
        event_count=session.event_count,
        created_at=session.created_at,
        last_active_at=session.last_active_at,
        metadata=session.metadata,
    )


@router.get("", response_model=SessionListResponse)
def list_sessions(
    current_user: dict = Depends(get_current_user),
    session_manager: SessionManager = Depends(get_session_manager),
    status: str | None = None,
    limit: int = 50,
    offset: int = 0,
):
    """List user's sessions with pagination and filtering.
    
    - **status**: Filter by status (active, closed)
    - **limit**: Max results (default 50, max 100)
    - **offset**: Skip N results for pagination
    """
    if limit > 100:
        limit = 100
    
    sessions = session_manager.list_sessions(
        user_id=current_user["user_id"],
        limit=limit,
        offset=offset,
    )
    
    # Filter by status if provided
    if status:
        sessions = [s for s in sessions if s.status.value == status]
    
    session_responses = [
        SessionResponse(
            session_id=s.session_id,
            user_id=s.user_id,
            status=s.status.value,
            event_count=s.event_count,
            created_at=s.created_at,
            last_active_at=s.last_active_at,
            metadata=s.metadata,
        )
        for s in sessions
    ]
    
    return SessionListResponse(
        sessions=session_responses,
        total=len(session_responses),
    )


@router.get("/{session_id}", response_model=SessionResponse)
def get_session(
    session_id: str,
    current_user: dict = Depends(get_current_user),
    session_manager: SessionManager = Depends(get_session_manager),
):
    """Get session by ID."""
    session = session_manager.get_session(session_id)
    
    if not session:
        raise HTTPException(
            status_code=status.HTTP_404_NOT_FOUND,
            detail="Session not found",
        )
    
    # Verify ownership
    if session.user_id != current_user["user_id"]:
        raise HTTPException(
            status_code=status.HTTP_403_FORBIDDEN,
            detail="Not authorized to access this session",
        )
    
    return SessionResponse(
        session_id=session.session_id,
        user_id=session.user_id,
        status=session.status.value,
        event_count=session.event_count,
        created_at=session.created_at,
        last_active_at=session.last_active_at,
        metadata=session.metadata,
    )


@router.delete("/{session_id}", status_code=status.HTTP_204_NO_CONTENT)
def close_session(
    session_id: str,
    current_user: dict = Depends(get_current_user),
    session_manager: SessionManager = Depends(get_session_manager),
):
    """Close a session."""
    session = session_manager.get_session(session_id)
    
    if not session:
        raise HTTPException(
            status_code=status.HTTP_404_NOT_FOUND,
            detail="Session not found",
        )
    
    # Verify ownership
    if session.user_id != current_user["user_id"]:
        raise HTTPException(
            status_code=status.HTTP_403_FORBIDDEN,
            detail="Not authorized to close this session",
        )
    
    session_manager.close_session(session_id)
