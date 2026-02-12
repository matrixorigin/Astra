"""FastAPI sessions router with SQLAlchemy."""

from datetime import datetime, timezone
from uuid import uuid4

from fastapi import APIRouter, Depends, HTTPException, status
from sqlalchemy.orm import Session

from api.dependencies import get_current_user
from api.database import get_db_session
from api.repositories.session_repository import SessionRepository
from schemas.session import SessionCreateRequest, SessionListResponse, SessionResponse

router = APIRouter()


@router.post("", response_model=SessionResponse, status_code=status.HTTP_201_CREATED)
def create_session(
    request: SessionCreateRequest,
    current_user: dict = Depends(get_current_user),
    db: Session = Depends(get_db_session),
):
    """Create a new session."""
    repo = SessionRepository(db)
    
    session_data = {
        "session_id": str(uuid4()),
        "user_id": current_user["user_id"],
        "status": "active",
        "event_count": 0,
        "created_at": datetime.now(timezone.utc),
        "last_active_at": datetime.now(timezone.utc),
        "session_metadata": request.metadata or {},
    }
    
    session = repo.create(session_data)
    
    return SessionResponse(
        session_id=session.session_id,
        user_id=session.user_id,
        status=session.status,
        event_count=session.event_count,
        created_at=session.created_at,
        last_active_at=session.last_active_at,
        metadata=session.session_metadata,
    )


@router.get("", response_model=SessionListResponse)
def list_sessions(
    current_user: dict = Depends(get_current_user),
    db: Session = Depends(get_db_session),
    status: str | None = None,
    limit: int = 50,
    offset: int = 0,
):
    """List user's sessions with pagination and filtering."""
    if limit > 100:
        limit = 100
    
    repo = SessionRepository(db)
    sessions = repo.list_by_user(
        user_id=current_user["user_id"],
        status=status,
        limit=limit,
        offset=offset,
    )
    
    return SessionListResponse(
        sessions=[
            SessionResponse(
                session_id=s.session_id,
                user_id=s.user_id,
                status=s.status,
                event_count=s.event_count,
                created_at=s.created_at,
                last_active_at=s.last_active_at,
                metadata=s.session_metadata,
            )
            for s in sessions
        ],
        total=len(sessions),
    )


@router.get("/{session_id}", response_model=SessionResponse)
def get_session(
    session_id: str,
    current_user: dict = Depends(get_current_user),
    db: Session = Depends(get_db_session),
):
    """Get session with ownership check."""
    repo = SessionRepository(db)
    session = repo.get_by_id(session_id, user_id=current_user["user_id"])
    
    if not session:
        raise HTTPException(
            status_code=status.HTTP_404_NOT_FOUND,
            detail="Session not found",
        )
    
    return SessionResponse(
        session_id=session.session_id,
        user_id=session.user_id,
        status=session.status,
        event_count=session.event_count,
        created_at=session.created_at,
        last_active_at=session.last_active_at,
        metadata=session.session_metadata,
    )


@router.post("/{session_id}/close", response_model=SessionResponse)
def close_session(
    session_id: str,
    current_user: dict = Depends(get_current_user),
    db: Session = Depends(get_db_session),
):
    """Close a session."""
    repo = SessionRepository(db)
    session = repo.update_status(session_id, current_user["user_id"], "closed")
    
    if not session:
        raise HTTPException(
            status_code=status.HTTP_404_NOT_FOUND,
            detail="Session not found",
        )
    
    return SessionResponse(
        session_id=session.session_id,
        user_id=session.user_id,
        status=session.status,
        event_count=session.event_count,
        created_at=session.created_at,
        last_active_at=session.last_active_at,
        metadata=session.session_metadata,
    )
