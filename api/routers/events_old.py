"""FastAPI events router with SQLAlchemy."""

from datetime import datetime, timezone
from uuid import uuid4

from fastapi import APIRouter, Depends, HTTPException, status
from sqlalchemy.orm import Session

from api.dependencies import get_current_user
from api.database import get_db_session
from api.repositories.event_repository import EventRepository
from api.repositories.session_repository import SessionRepository
from schemas.event import EventCreateRequest, EventListResponse, EventResponse

router = APIRouter()


@router.post("", response_model=EventResponse, status_code=status.HTTP_201_CREATED)
def create_event(
    request: EventCreateRequest,
    current_user: dict = Depends(get_current_user),
    db: Session = Depends(get_db_session),
):
    """Create a new event."""
    # Check session exists and belongs to user
    session_repo = SessionRepository(db)
    session = session_repo.get_by_id(request.session_id, user_id=current_user["user_id"])
    
    if not session:
        raise HTTPException(
            status_code=status.HTTP_404_NOT_FOUND,
            detail="Session not found",
        )
    
    # Create event
    event_repo = EventRepository(db)
    event_data = {
        "event_id": str(uuid4()),
        "session_id": request.session_id,
        "user_id": current_user["user_id"],
        "event_type": request.event_type,
        "content": request.content,
        "created_at": datetime.now(timezone.utc),
        "event_metadata": request.metadata or {},
    }
    
    event = event_repo.create(event_data)
    
    # Update session event count
    session.event_count += 1
    session.last_active_at = datetime.now(timezone.utc)
    db.commit()
    
    return EventResponse(
        event_id=event.event_id,
        session_id=event.session_id,
        user_id=event.user_id,
        event_type=event.event_type,
        content=event.content,
        created_at=event.created_at,
        metadata=event.event_metadata,
    )


@router.get("", response_model=EventListResponse)
def list_events(
    session_id: str,
    current_user: dict = Depends(get_current_user),
    db: Session = Depends(get_db_session),
    event_type: str | None = None,
    limit: int = 100,
    offset: int = 0,
):
    """List events for a session with pagination and filtering."""
    if limit > 500:
        limit = 500
    
    # Check session exists and belongs to user
    session_repo = SessionRepository(db)
    session = session_repo.get_by_id(session_id, user_id=current_user["user_id"])
    
    if not session:
        raise HTTPException(
            status_code=status.HTTP_404_NOT_FOUND,
            detail="Session not found",
        )
    
    # List events
    event_repo = EventRepository(db)
    events = event_repo.list_by_session(
        session_id=session_id,
        user_id=current_user["user_id"],
        event_type=event_type,
        limit=limit,
        offset=offset,
    )
    
    return EventListResponse(
        events=[
            EventResponse(
                event_id=e.event_id,
                session_id=e.session_id,
                user_id=e.user_id,
                event_type=e.event_type,
                content=e.content,
                created_at=e.created_at,
                metadata=e.event_metadata,
            )
            for e in events
        ],
        total=len(events),
    )


@router.get("/{event_id}", response_model=EventResponse)
def get_event(
    event_id: str,
    current_user: dict = Depends(get_current_user),
    db: Session = Depends(get_db_session),
):
    """Get event with ownership check."""
    event_repo = EventRepository(db)
    event = event_repo.get_by_id(event_id, user_id=current_user["user_id"])
    
    if not event:
        raise HTTPException(
            status_code=status.HTTP_404_NOT_FOUND,
            detail="Event not found",
        )
    
    return EventResponse(
        event_id=event.event_id,
        session_id=event.session_id,
        user_id=event.user_id,
        event_type=event.event_type,
        content=event.content,
        created_at=event.created_at,
        metadata=event.event_metadata,
    )
